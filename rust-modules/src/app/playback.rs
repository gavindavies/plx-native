//! Playback orchestration from the app core's side: the scrub/seek/position accessors, the
//! start/exit/finish of a playback, and the performers behind the requests the player page raises
//! ([`player_requests`]). Moved out of `app.rs` verbatim in phase 1a (a pure move; `pub(crate)`
//! widening only).
//!
//! **The HUD's timer, its cursor, the scrub gesture and the repeat gates are no longer here.**
//! Restructure phase 9 moved them into the player's own `Screen` instance
//! (`plx_screens::player`), with the types and the pure predicates in
//! `plx_screens::player::input`; this module re-exports the few names the loop still spells
//! unqualified. What that changed for a reader of the code below: `hud_until()`/`set_hud`/
//! `extend_hud`/`scrub()`/`set_scrub` are gone as free functions, because the values they read and
//! wrote are fields of a screen now (§2.3), and every arm that touches them takes that screen.
//!
//! **And the player's KEY HANDLERS are no longer here either** (phase 12, PX-PLAYER).
//! `key_player_failed`, `key_player_updown`, `key_scrub` and `seed_scrub` are deleted, not moved:
//! `PlayerScreen::step` had been answering the same keys through the dispatcher since the page
//! began reporting `FocusSource::Engine`, and these four went on writing that page's `hud`/`scrub`
//! from the loop beside it. What is left of the player here is the half a screen may not do —
//! [`commit_seek`], [`exit_player`], [`activate_player_row`], the transport toggle — each reached
//! through a `PlayerReq`.

use super::*;
// The list is what the LOOP still spells, and phase 12 (PX-PLAYER) is why it is short: the scrub
// gesture's constants and its two pure policies (`scrub_press`, `failed_key_action`) are read by
// `screens::player` alone now, so re-exporting them here would be this module claiming a
// vocabulary it no longer uses.
pub(crate) use plx_screens::player::input::{
    HudNav, HudState, Scrub, HUD_LINGER_MS,
};
// The modal repeat cadence is `screens::registry`'s (phase 10 merge): the item context menu is a
// surface of its own family and needs the same gate, so it is shared vocabulary rather than the
// player's. `App::modal_repeat` still reaches it through this re-export.
pub(crate) use plx_screens::registry::RepeatGate;


#[inline]
pub(crate) fn resume_pend() -> bool {
    plx_media::player::TX.resume_pend.load(Relaxed)
}
#[inline]
pub(crate) fn set_resume_pend(v: bool) {
    plx_media::player::TX.resume_pend.store(v, Relaxed)
}
#[inline]
pub(crate) fn playpos() -> i64 {
    plx_media::player::playpos_ns()
}
#[inline]
pub(crate) fn frames() -> i32 {
    plx_media::player::frames()
}
#[inline]
pub(crate) fn seek_pending() -> i64 {
    plx_media::player::seek_pending()
}
#[inline]
pub(crate) fn request_seek(x: i64) {
    plx_media::player::request_seek(x)
}
/// Commit a scrub to `target` and clear the preview. If we were PAUSED, STAY logically paused: a
/// dedicated seek-preroll feed override lets the synchronized native clock decode one landed frame
/// without publishing a false viewer Resume. `resume_pend` asks the per-frame loop to close that
/// bounded override. `repause_at` is the landed-frame wait target.
pub(crate) fn commit_seek(target: i64, repause_at: &mut i64) {
    request_seek(target);
    if paused() {
        *repause_at = target;
        set_resume_pend(true);
        plx_media::player::TX.begin_paused_seek();
    }
}
#[inline]
pub(crate) fn is_started() -> bool {
    plx_media::player::is_started()
}

/// **Try a failed file again**: a REAL stop followed by a new start of the same file, at the
/// position the viewer was at (the per-file progress record). `false` when the failed session has
/// no file to start again.
pub(crate) fn retry_failed_playback(
    ps: &mut plx_media::playback::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) -> bool {
    // Read BEFORE teardown, which clears the URL.
    let Some(path) = plx_media::localfile::path_of(&plx_media::playback::url(ps)) else {
        log("playback retry: the failed session has no file");
        return false;
    };
    plx_media::player::stop_bufferfeed(ps, pa);
    super::local_watch::play(ps, pa, &path, false, None, pages, bridge);
    plx_machine::idle::invalidate();
    true
}

/// **Put the player on the page stack, and record where BACK, Stop and EOS land** (§5.1).
///
/// The origin is the entry that is on top RIGHT NOW — the page the user pressed Play on — seeded
/// for the mount the push performs and read back off the mounted screen at the exit. It replaces
/// `App.play_from: Node` plus `enter_node` plus `Trail::ensure`, which between them said "re-derive
/// a page from a remembered description, then make the history agree".
///
/// **A restart is not a navigation, and this is where that is decided.** `NavOp::Push` always
/// MINTS an entry — it has no reuse rule (`NavStack::apply`) — so re-entering with the player
/// already on top would stack a SECOND player over the first and mount a fresh screen that
/// consumed no seed. The page the user is standing on has not changed; only the media has.
///
/// Its own function so that the page-level half of the ritual is reachable without an engine —
/// `player_return_tests` drives exactly this, over a real container.
pub(crate) fn enter_player(
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
    ret: Option<plx_ui::screen::ReturnState<u32, plx_screens::registry::PageMemory>>,
) {
    // …or already on its way: a Play inside the push's own dip-out finds the page it pressed on
    // still the committed top, and re-seeding from it would leave a seed no mount ever spends
    // (the pending push is inert against its twin, `NavStack::is_inert`) for the next player
    // mount to pick up as its origin.
    let already_up = pages
        .nav
        .top_page()
        .map(|e| matches!(e.arg, AppArg::Player))
        .unwrap_or(false)
        || matches!(pages.nav.tabs.stack.pending_dest(), Some(AppArg::Player));
    if already_up {
        return;
    }
    if let Some(entry) = pages.nav.top_page() {
        bridge.seed_player_origin(plx_screens::player::Origin {
            entry: entry.id,
            instance: entry.inst.as_ref().map(|i| i.id),
        });
    }
    match ret {
        Some(ret) => super::bridge::nav_push_with_return(pages, AppArg::Player, ret),
        None => super::bridge::nav_push(pages, AppArg::Player),
    }
}
/// The ONE start-playback ritual: arm the resume point BEFORE the first Load, start the engine,
/// record the Stop/BACK/EOS return target, reset the HUD focus cursor, and show the HUD.
pub(crate) fn start_playback(
    ps: &mut plx_media::playback::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    resume_ns: i64,
    hud_ms: u32,
    ret: Option<plx_ui::screen::ReturnState<u32, plx_screens::registry::PageMemory>>,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) -> bool {
    let entering = prepare_start(ps, pa, resume_ns);
    if entering {
        enter_player(pages, bridge, ret);
    }
    // A NEW session starts on the scrubber with its transport pinned for `hud_ms`, and both halves
    // of that are the INSTANCE's: a fresh mount is built at `HudState::IDLE`.
    //
    // **Both paths, because there are two.** An ordinary start pushes the page and it mounts next
    // frame, so the pin is SEEDED for that mount; an auto-advance re-enters here with the player
    // already on screen, so the live instance is reset in place. Stamped from NOW rather than from
    // the keypress, so a slow open cannot expire the HUD before it is ever drawn.
    bridge.seed_player_hud(hud_ms);
    if let Some(player) = super::bridge::player_mut(pages) {
        player.hud = HudState::IDLE;
        player.scrub = Scrub::IDLE;
        player.hud.extend(clock::now(), hud_ms);
        player.publish();
    }
    set_paused(false);
    entering
}

/// Arm the resume point and start the engine.
fn prepare_start(ps: &mut plx_media::playback::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter, resume_ns: i64) -> bool {
    match plx_media::player::begin_resume(ps, resume_ns) {
        plx_media::player::ResumeOutcome::Prepared => plx_media::player::start_bufferfeed(ps, pa),
        plx_media::player::ResumeOutcome::NoRoute => false,
    }
}

/// Leaving playback (Stop / BACK / EOS): retire every in-player panel. They are entries on the
/// player page's own `ModalStack`, and unmounting the page unmounts them with it; the dismissal is
/// here so the exit takes every panel down in the same frame.
pub(crate) fn close_player_overlays(pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>) {
    super::bridge::dismiss_player_overlays(pages);
}
/// **Perform the track menu's pick.**
///
/// `route::commit_audio_selection`/`commit_subtitle_selection` take the playback session's `&mut`,
/// which no screen has (§2.2) — the panel returns a
/// [`TrackCommit`](plx_appkit::track_menu::TrackCommit) and this is where it lands, from the
/// overlay's `PlayerReq` and from the headless `pitv-menupick` trigger alike.
pub(crate) fn commit_track(
    ps: &mut plx_media::playback::PlaybackSession,
    commit: plx_appkit::track_menu::TrackCommit,
) {
    use plx_appkit::track_menu::TrackCommit;
    match commit {
        // A track the sink cannot decode cannot be played: say so.
        TrackCommit::Audio(audio) if plx_media::player::engine::audio_payload_codec(&audio.codec).is_none() =>
        {
            plx_base::eventlog::log(&format!("local audio: {} is not decodable here; pick refused", audio.codec));
            super::toast::raise(plx_media::localplay::LocalPlayError::AudioCodecs(vec![audio.codec]).message());
        }
        TrackCommit::Audio(audio) => plx_media::playback::commit_audio_selection(ps, audio),
        TrackCommit::Subtitle { render_ordinal, stream_id, sidecar_key, sidecar_codec } => {
            plx_media::playback::commit_subtitle_selection(ps, render_ordinal, stream_id);
            // An EXTERNAL pick has no demuxer ordinal (`render_ordinal` is -1, so the embedded
            // renderer is off): `player::sidecar` reads the file beside the film and draws it.
            match sidecar_key {
                Some(key) => plx_media::player::sidecar::select_file(stream_id, key, sidecar_codec),
                None => plx_media::player::sidecar::deselect(),
            }
        }
        TrackCommit::SubtitleTone(tone) => plx_media::player::set_subtitle_tone(tone),
        // the live value and `settings.json`, both at once (`plx_data::settings`)
        TrackCommit::SubtitleSize(size) => plx_data::settings::set_subtitle_size(size),
        TrackCommit::SubtitlePosition(position) => plx_data::settings::set_subtitle_position(position),
        // per file: `local_watch` records the offset with the file's other choices
        TrackCommit::SubtitleOffset(offset) => plx_media::player::set_subtitle_offset(offset),
    }
}

pub(crate) fn player_requests(
    ps: &mut plx_media::playback::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    reqs: Vec<plx_screens::registry::PlayerReq>,
    now: u32,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
    ok_armed: &mut bool,
    press: &mut plx_ui::press::Press,
    repause_at: &mut i64,
) {
    use plx_screens::registry::PlayerReq;
    for req in reqs {
        match req {
            PlayerReq::RetryPlayback => {
                retry_failed_playback(ps, pa, pages, bridge);
            }
            PlayerReq::ExtendHud(ms) => {
                if let Some(player) = super::bridge::player_mut(pages) {
                    player.hud.extend(now, ms);
                    player.publish();
                }
            }
            PlayerReq::FocusMenu => {
                let has_chapters = plx_appkit::chapters_panel::has_chapters(bridge.metadata_view());
                if let Some(player) = super::bridge::player_mut(pages) {
                    player.hud.nav.focus = 1;
                    if let Some(item) = player.menu_open.and_then(|m| plx_appkit::player_hud::menu_index(m, has_chapters)) {
                        player.hud.nav.btn = item;
                    }
                }
            }
            // The fall-through a surface cannot perform (`screens::player::overlay`'s module doc):
            // the same toggle the bare transport reaches, with the panel left untouched.
            PlayerReq::Transport(play) => {
                set_transport_paused(pa, super::lifecycle::transport_target(play, super::lifecycle::viewer_paused()));
                if let Some(player) = super::bridge::player_mut(pages) {
                    player.hud.extend(now, HUD_LINGER_MS);
                    player.publish();
                }
            }
            PlayerReq::SeekTo(ns) => {
                request_seek(ns);
                resume_if_paused(pa);
            }
            // …and its twin, which does NOT resume: the scrub bar is how a paused film is moved.
            // `repause_at` is an `App` field, which is exactly why this is a request.
            PlayerReq::CommitSeek(ns) => commit_seek(ns, repause_at),
            PlayerReq::CommitTrack(commit) => commit_track(ps, commit),
            // A panel presented immediately rather than through the deferred press
            // `ArmControlRow` uses (the Tracks panel handing off to Timing).
            PlayerReq::OpenOverlay(kind) => {
                super::bridge::open_player_overlay(bridge.metadata_view(), pages, kind);
            }
            // The old `key_ok`'s `focus == 1` arm: dip the tvOS press for the transport's control
            // row. The loop's
            // existing commit-frame dispatch (`Route::Player => activate_player_row(...)`, below
            // in this same call chain) is unchanged and performs whatever the row decides.
            PlayerReq::ArmControlRow => {
                press.begin_ctl(now);
                *ok_armed = true;
            }
            // The STOP key, a BACK with nothing else open and the failure read-out's own BACK
            // escape all reach the same ritual.
            PlayerReq::Exit => {
                exit_player(ps, pa, pages);
            }
        }
    }
}

/// Shared return decision after playback ends; resource teardown belongs to `exit_player`.
pub(super) fn return_from_player(
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
) {
    match super::bridge::player(pages).and_then(|p| p.origin) {
        Some(origin) => super::bridge::nav_pop_to(pages, origin.entry),
        // No recorded origin: `nav_pop_to`'s own floor, the library's root.
        None => super::bridge::nav_pop_to(pages, plx_machine::machine::EntryId(u32::MAX)),
    }
}

/// The ONE leave-playback ritual (Stop key, BACK, EOS): close the overlays, stop the engine and put
/// the page the session was LAUNCHED FROM back on screen.
///
/// The return target is [`plx_screens::player::Origin`]'s `entry` — the container entry that was
/// on top when Play was pressed, seeded at the player's own mount by [`enter_player`] and read back
/// off the mounted screen here. Re-entry is `NavOp::PopTo(entry)`, the same container op a BACK
/// off a stacking page performs.
pub(crate) fn exit_player(
    ps: &mut plx_media::playback::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
) {
    close_player_overlays(pages);
    plx_media::player::stop_bufferfeed(ps, pa);
    return_from_player(pages);
}

/// Activate the player transport's focused CONTROL ROW item — the deferred half of [`key_ok`]'s
/// player arm, run from the per-frame loop once the press spring-back has played.
///
/// OK on a control disc PRESENTS its panel on this page's own stack: the Subtitles disc opens
/// the track menu on its Subtitles tab, the Audio disc on its Audio tab.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn activate_player_row(
    now: u32,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) {
    use plx_appkit::player_hud::MenuItem;
    use plx_screens::player::overlay::OverlayKind;
    // The cursor is the instance's; read the one field this dispatch turns on, so the container
    // is borrowed once and the arms below are free to present a panel on it.
    let btn = super::bridge::player(pages).map_or(0, |player| player.hud.nav.btn);
    let has_chapters = plx_appkit::chapters_panel::has_chapters(bridge.metadata_view());
    let kind = match plx_appkit::player_hud::menu_item(btn, has_chapters) {
        Some(MenuItem::Info) => OverlayKind::Info,
        Some(MenuItem::Chapters) => OverlayKind::Chapters,
        Some(MenuItem::Subtitles) => OverlayKind::Tracks { tab: 1 },
        Some(MenuItem::Audio) => OverlayKind::Tracks { tab: 0 },
        None => return,
    };
    super::bridge::open_player_overlay(bridge.metadata_view(), pages, kind);
    if let Some(player) = super::bridge::player_mut(pages) {
        player.hud.extend(now, HUD_LINGER_MS);
        player.publish();
    }
}

/// PAUSE — the dedicated transport key, which only ever pauses (PLAY is its other half).
pub(crate) fn key_pause(
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    now: u32,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
) {
    if super::bridge::player(pages).is_some() && !paused() {
        set_transport_paused(pa, true);
    }
    if let Some(player) = super::bridge::player_mut(pages) {
        player.hud.extend(now, HUD_LINGER_MS);
        player.publish();
    }
}

/// PLAY — off the player route it starts the buffer-feed and enters the player; on it, it un-pauses.
pub(crate) unsafe fn key_play(
    ps: &mut plx_media::playback::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    now: u32,
    ptr: &mut Pointer,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) {
    let was_off_player = super::bridge::player(pages).is_none();
    if was_off_player {
        // A session that is loaded but not on screen (its page was left with the engine still up)
        // is put back on screen and resumed.
        if plx_media::player::start_bufferfeed(ps, pa) {
            // The origin is the page the PLAY key was pressed on — through the same one door
            // `start_playback` uses, so the seed and the push cannot drift apart here.
            enter_player(pages, bridge, None);
            if paused() {
                set_transport_paused(pa, false);
            }
        }
    } else if paused() {
        set_transport_paused(pa, false);
    }
    if was_off_player && !ptr.dpad_mode {
        ptr.dpad_mode = true;
        ptr.cur_hidden = true;
    }
    if let Some(player) = super::bridge::player_mut(pages) {
        player.hud.extend(now, HUD_LINGER_MS);
        player.publish();
    }
}

// (`key_scrub` and `seed_scrub` stood here — the LEFT/RIGHT ladder for the player route and the
// in-flight-target seed a fresh gesture starts from. Both are `PlayerScreen`'s since phase 12
// (PX-PLAYER): `key_scrub_fresh` runs the same `ScrubPress` dispatch, and the seed is
// `player::intended_pos_ns` read at the same moment for the same reason — while a prior commit's
// seek is still landing, `playpos()` reports the PRE-seek spot, so a quick re-press seeded off it
// would jump back to where the last one started.
//
// The rules those two docs carried are not lost, and they are worth restating where the behaviour
// now lives: a press that finds the HUD HIDDEN is spent RAISING it and moves nothing (the
// transport sits on a 4.5 s timer over full-screen video, so "where am I" and "take me back ten
// seconds" are two intentions the remote cannot tell apart, and reading every LEFT as the second
// cost a viewer their place); and a HOLD is not a tap, so the reveal still arms the gesture and
// the ordinary continuous scrub grows out of it as the repeats arrive. `ScrubPress` and
// `screens::player::input`'s own `a_hidden_hud_spends_the_press_on_itself` are where both are
// pinned.)

/// **The loop's half of the scrub commit: a paused film stays paused across the seek.**
///
/// `PlayerReq::CommitSeek` promises this and `screens::player`'s own
/// `a_scrub_seek_taken_while_paused_holds_the_pause` grades that the screen ASKS for it; this
/// grades what the arm then does, which is the half no screen test can reach — `repause_at` is an
/// `App` field, `resume_pend` and the seek-preroll override are the pipeline's, and none of the
/// three is nameable from `screens/` (`ci/check-deps.sh`'s `layer` gate).
#[cfg(test)]
mod paused_seek_tests {
    use super::*;

    /// Every state this touches is a crate global, so the body holds `testlock::serial()`.
    #[test]
    fn a_scrub_commit_taken_while_paused_arms_the_repause_instead_of_resuming() {
        let _g = plx_base::testlock::serial();
        let was_paused = plx_media::player::TX.paused.load(Relaxed);
        let (was_seek, was_resume, was_seeking) = (seek_pending(), resume_pend(), plx_media::player::SHARED.seeking.load(Relaxed));
        plx_media::player::TX.paused.store(true, Relaxed);
        let mut repause_at = 0i64;

        commit_seek(42_000_000_000, &mut repause_at);

        assert_eq!(seek_pending(), 42_000_000_000, "the seek really was requested");
        assert_eq!(repause_at, 42_000_000_000, "…and the landed-frame wait target is its own");
        assert!(resume_pend(), "the per-frame loop is asked to close the bounded override");
        assert!(
            plx_media::player::seek_preroll_active(),
            "…which is what lets the pipeline decode the landed frame with the transport still \
             saying Paused — the whole difference from PlayerReq::SeekTo",
        );

        plx_media::player::TX.finish_seek_preroll();
        plx_media::player::TX.paused.store(was_paused, Relaxed);
        // Restore what the commit wrote: a seek left in flight reads as `Seeking` to every
        // later test of this binary, which used to be cleaned up by the player's own tests that ran
        // in the same binary before the media layer became a crate of its own.
        plx_media::player::TX.seek_to_ns.store(was_seek, Relaxed);
        plx_media::player::SHARED.seeking.store(was_seeking, Relaxed);
        set_resume_pend(was_resume);
    }

    /// …and the complement: while PLAYING there is nothing to hold, so the commit is a bare seek
    /// and arms no re-pause at all. Without this the case above passes for a `commit_seek` that
    /// armed the override unconditionally, which would re-pause a film nobody had paused.
    #[test]
    fn a_scrub_commit_taken_while_playing_arms_no_repause() {
        let _g = plx_base::testlock::serial();
        let was_paused = plx_media::player::TX.paused.load(Relaxed);
        let (was_seek, was_resume, was_seeking) = (seek_pending(), resume_pend(), plx_media::player::SHARED.seeking.load(Relaxed));
        plx_media::player::TX.paused.store(false, Relaxed);
        set_resume_pend(false);
        let mut repause_at = 7i64;

        commit_seek(11_000_000_000, &mut repause_at);

        assert_eq!(seek_pending(), 11_000_000_000);
        assert_eq!(repause_at, 7, "untouched: there is no pause to hold");
        assert!(!resume_pend());
        assert!(!plx_media::player::seek_preroll_active());
        plx_media::player::TX.paused.store(was_paused, Relaxed);
        plx_media::player::TX.seek_to_ns.store(was_seek, Relaxed);
        plx_media::player::SHARED.seeking.store(was_seeking, Relaxed);
        set_resume_pend(was_resume);
    }
}

/// **Where playback returns to**: the `EntryId` of the page that was on top when Play was
/// pressed, seeded at the player's own mount and read back off the mounted screen, over a real
/// container.
#[cfg(test)]
mod player_return_tests {
    use super::super::bridge::{self, AppHost, Bridge};
    use super::{enter_player, return_from_player};
    use plx_screens::registry::{AppArg, LocalArg};
    use plx_ui::dispatch::Dispatcher;
    use plx_ui::fixture::tick;
    use plx_ui::screen::ScreenArg;

    fn folder(path: &str) -> AppArg {
        AppArg::Local(LocalArg { path: path.into() })
    }

    struct Pages {
        d: Dispatcher<AppHost>,
        rig: Bridge,
        t: u32,
    }

    impl Pages {
        fn new() -> Self {
            Self { d: Dispatcher::<AppHost>::new(), rig: Bridge::for_test(|| 0), t: 0 }
        }

        /// Enough frames for a `PageDip` to reach its floor, apply the op and ramp back up.
        fn settle(&mut self) {
            for _ in 0..24 {
                self.t += 16;
                bridge::frame(&mut self.d, &mut self.rig, tick(self.t), vec![]);
            }
        }

        fn stand_on(&mut self, arg: AppArg) -> &mut Self {
            bridge::show_page(&mut self.d, arg);
            self.settle();
            self
        }

        fn top(&self) -> AppArg {
            self.d.top_arg().cloned().expect("a page is on top")
        }

        fn top_entry(&self) -> plx_machine::machine::EntryId {
            self.d.nav.top_page().expect("a page is on top").id
        }

        /// **Press Play** — production's own page-level ritual, called directly.
        fn play(&mut self) {
            enter_player(&mut self.d, &mut self.rig, None);
            self.settle();
        }

        /// **Stop / BACK / EOS** at the shared production return boundary.
        fn back_out(&mut self) {
            return_from_player(&mut self.d);
            self.settle();
        }
    }

    /// **You come back to the folder you were standing on** — the same entry, not a second copy.
    #[test]
    fn a_session_returns_to_the_folder_it_was_launched_from() {
        let _serial = plx_base::testlock::serial();
        for launched_from in [folder(""), folder("/media/films")] {
            let mut p = Pages::new();
            p.stand_on(folder(""));
            p.stand_on(launched_from.clone());
            let before = p.top_entry();
            p.play();
            assert!(matches!(p.top(), AppArg::Player), "the player is the page that was pushed");
            p.back_out();
            assert!(p.top().same_instance(&launched_from));
            assert_eq!(p.top_entry(), before, "…and to the SAME entry");
        }
    }

    /// **A restart is not a navigation**: re-entering with the player already on top keeps the
    /// one player and the origin the user came from.
    #[test]
    fn a_restart_keeps_the_page_the_user_came_from() {
        let _serial = plx_base::testlock::serial();
        let mut p = Pages::new();
        p.stand_on(folder("")).stand_on(folder("/media/shows/s1"));
        let origin = p.top_entry();
        p.play();
        let player = p.top_entry();
        p.play();
        assert_eq!(p.top_entry(), player, "no second player was stacked");
        p.back_out();
        assert_eq!(p.top_entry(), origin);
    }

    /// **An origin that is no longer on the stack falls back to the root** — Home, the tab bar's
    /// first pill — rather than to nothing.
    #[test]
    fn an_origin_that_is_gone_falls_back_to_the_root() {
        let _serial = plx_base::testlock::serial();
        let mut p = Pages::new();
        p.stand_on(folder(""));
        p.rig.seed_player_origin(plx_screens::player::Origin {
            entry: plx_machine::machine::EntryId(9_999),
            instance: None,
        });
        bridge::nav_push(&mut p.d, AppArg::Player);
        p.settle();
        p.back_out();
        assert!(p.top().same_instance(&AppArg::Home), "a stranded return lands on the root, Home");
    }
}

