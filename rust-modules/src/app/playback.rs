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
use plx_media::player::report::AttemptOrigin;
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
/// The playhead the user INTENDED, which is not always the one being published. While a seek is
/// still resolving (request → reopen → prime → Play) `playpos()` keeps reporting the PRE-seek spot,
/// so anything that snapshots "where are we?" inside that window snapshots the position the user
/// just left. The rule — an in-flight seek target wins, else the published position — was open-coded
/// at each reader that remembered it (the scrub seed below; the HUD's frozen playhead in
/// `appkit/player_hud.rs`) and simply MISSING at the one that did not: the OS-background save took a bare
/// `playpos()`, so backgrounding right after a seek stored the pre-seek spot and the foreground
/// restore replayed from there — and teardown clears the pending target, so nothing self-corrected.
/// Use this at every reader that means "where the user is"; keep the raw `playpos()` only where the
/// PUBLISHED position is the point (the re-pause gate, which is already behind `seek_pending() < 0`,
/// and the heartbeat's `pos=`, which the harness grades real playback progress from).
#[inline]
pub(crate) fn intended_pos(ps: &plx_media::route::PlaybackSession) -> i64 {
    plx_media::player::intended_pos_ns(ps)
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
    plx_telemetry::diag::event(plx_telemetry::diag::schema::DiagEvent::FeatureUsed {
        feature: plx_telemetry::diag::schema::Feature::Seek,
    });
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

// ---- the route vocabulary, and the pure questions asked ABOUT a route -------------------------
//
// These are pure functions of a `Route` that read and write no app state, which is what lets
// `route_tests` at the bottom of this file grade them — and grading them is the point, because they
// decide things that have shipped wrong (the teardown rule below, twice), and a `Route` that only
// exists inside the run loop's body is a decision no host test can reach. The loop still owns every
// VALUE — `route` is a local, the trail is a local.

/// Perform what the `…` popover reported. Shared by the OK key and the pointer click, so
/// the two paths can never come to disagree about what a row does.
pub(crate) fn apply_more_action(ps: &mut plx_media::route::PlaybackSession, pa: &mut plx_media::player::adapter::PlayerAdapter, bridge: &mut super::bridge::Bridge, a: plx_appkit::more_menu::Action) {
    match a {
        // A rung of the playback-quality ladder — a routing POLICY, not a number handed to a
        // running stream: `route::set_quality` re-asks the routing question for the playback on
        // screen and reloads only when the answer changed.
        plx_appkit::more_menu::Action::SetQuality(q) => {
            let failed = matches!(plx_media::player::state(ps), plx_media::player::PlaybackState::Error);
            if failed {
                plx_media::route::set_quality_for_retry(q);
                retry_failed_playback(ps, pa, bridge.metadata_mut(), None, AttemptOrigin::QualityChange);
            } else {
                plx_media::route::set_quality(ps, q);
            }
        }
        plx_appkit::more_menu::Action::ToggleStats
        | plx_appkit::more_menu::Action::SendDiagnostics
        | plx_appkit::more_menu::Action::None => {}
    }
}
/// Replace a terminal attempt with a new resolve of the same Plex item.
///
/// This is a REAL stop followed by a new request, not an Engine reload: it covers the pre-flight
/// refusal which never created an Engine, retires a failed server transcode when there was one,
/// and gives telemetry two honest attempts.  The descriptor lives in `route`; the app owns only
/// the current playhead and the Engine lifecycle.
///
/// `direct_play` overrides the Direct Play mode the retry resolves under; `None` keeps the failed
/// attempt's own (a retry is the SAME request). `origin` is why the viewer asked — a Retry press
/// or a quality pick on the failure screen — and rides to the error report of the attempt this
/// starts, which is the next open of the same session.
pub(crate) fn retry_failed_playback(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    meta: &mut plx_data::stores::metadata::MetadataStore,
    direct_play: Option<plx_plex::plex::session::DirectPlayMode>,
    origin: AttemptOrigin,
) -> bool {
    // URL/dev-trigger playback has no Plex descriptor.  Check BEFORE teardown: extinguishing its
    // Error Engine and only then discovering it cannot be rebuilt would replace an actionable
    // read-out with an idle black frame.
    if !plx_media::route::can_retry_current_play(ps) {
        log("playback retry: current source has no reusable Plex request");
        return false;
    }
    // A terminal error can race a seek whose requested target has not landed.  Resume what the
    // viewer asked for, not the last frame the dying Engine happened to publish.  If an earlier
    // retry was refused before presenting anything, retain its target too: the stopped Engine now
    // reports zero and must not send a second quality attempt back to the beginning.
    let resume_ns = intended_pos(ps)
        .max(plx_media::route::unpresented_resume_ns(ps))
        .max(0);
    plx_media::player::stop_bufferfeed(ps, pa);
    if plx_media::route::retry_current_play(ps, meta, resume_ns, direct_play, origin) {
        plx_machine::idle::invalidate();
        true
    } else {
        log("playback retry: current source cannot be resolved again");
        false
    }
}

/// **The failure read-out's fix for Force Direct Play** — `FailureAction::PlayAutomatically`.
///
/// A PERSISTENT switch, deliberately, not a one-shot: Force is what made this play fail, and it
/// would make the next unsupported file fail the same way. The preference moves to Auto now (the
/// in-memory value every later resolve reads) and is saved on the storage worker behind it, both
/// by the one write door (`route::choose`, `How::Optimistic`): the frame may not block on storage,
/// the cell is published ONCE, and a failed save is logged by the door while the pick stands for
/// this session. The retry resolves under Auto explicitly rather than inheriting the failed
/// attempt's Force, which a plain retry keeps on purpose.
pub(crate) fn play_automatically(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    meta: &mut plx_data::stores::metadata::MetadataStore,
) -> bool {
    use plx_plex::plex::session::{DirectPlayMode, DirectPlayModePref};
    plx_media::route::choose::<DirectPlayModePref>(DirectPlayMode::Auto, plx_media::route::How::Optimistic(None));
    retry_failed_playback(ps, pa, meta, Some(DirectPlayMode::Auto), AttemptOrigin::UserRetry)
}

/// **Where a session that is STARTING returns to** — the whole of what `app::nav::Origin` was,
/// once the origin became an `EntryId` rather than a described page.
///
/// It carried an `Origin::From(Node)` — a whole history entry, re-derived from the live stores at
/// every launch site by `origin_here` — because a `Route` names a KIND of page and BACK has to
/// land on the RIGHT one. The container already holds the right one: it is the entry on top when
/// Play was pressed, which is why this is a two-value CHOICE now rather than a payload.
///
/// [`Self::Unchanged`] is the auto-advance rule and the reason this is not a bool at a call site:
/// `play_up_next` starts a new item while the player is already up (so "the page on screen" is the
/// player, and the user chose nothing), and a PLAY key that resumes a session the app-switch
/// lifecycle suspended is resuming the same session from a page it was never launched from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Origin {
    /// A fresh launch: Stop/BACK/EOS lands on the page that is on top right now.
    Here,
    /// Keep whatever the live session already returns to.
    Unchanged,
}

/// **Put the player on the page stack, and record where BACK, Stop and EOS land** (§5.1).
///
/// The origin is the entry that is on top RIGHT NOW — the page the user pressed Play on — seeded
/// for the mount the push performs and read back off the mounted screen at the exit. It replaces
/// `App.play_from: Node` plus `enter_node` plus `Trail::ensure`, which between them said "re-derive
/// a page from a remembered description, then make the history agree".
///
/// **An auto-advance is not a navigation, and this is where that is decided.** `NavOp::Push`
/// always MINTS an entry — it has no reuse rule (`NavStack::apply`) — so re-entering with the
/// player already on top would stack a SECOND player over the first, mount a fresh screen that
/// consumed no seed, and land the exit on Home from the second episode of every chain. The page
/// the user is standing on has not changed; only the media has. `Origin::Unchanged` says the same
/// thing about the SEED, and the two agree at every call site; both are stated because they are
/// answers to different questions (which page is on top, and whose choice this was).
///
/// Its own function so that the page-level half of the ritual is reachable without an engine —
/// `player_return_tests` drives exactly this, over a real container.
pub(crate) fn enter_player(
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
    from: Origin,
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
    if matches!(from, Origin::Here) {
        if let Some(entry) = pages.nav.top_page() {
            bridge.seed_player_origin(plx_screens::player::Origin {
                entry: entry.id,
                instance: entry.inst.as_ref().map(|i| i.id),
            });
        }
    }
    match ret {
        Some(ret) => super::bridge::nav_push_with_return(pages, AppArg::Player, ret),
        None => super::bridge::nav_push(pages, AppArg::Player),
    }
}
/// The ONE start-playback ritual: arm the resume point BEFORE the first Load, start the engine,
/// record the Stop/BACK/EOS return target, reset the HUD focus cursor, and show the HUD.
pub(crate) fn start_playback(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    resume_ns: i64,
    from: Origin,
    hud_ms: u32,
    ret: Option<plx_ui::screen::ReturnState<u32, plx_screens::registry::PageMemory>>,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) -> bool {
    let entering = prepare_start(ps, pa, resume_ns);
    if entering {
        enter_player(pages, bridge, from, ret);
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
        player.up_next.reset();
        player.hud.extend(clock::now(), hud_ms);
        player.publish();
    }
    set_paused(false);
    entering
}

/// Arm the resume point and start the engine, or defer both to the play landing while a resolve
/// is in flight.
fn prepare_start(ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter, resume_ns: i64) -> bool {
    // A resolve in flight means the route is NOT installed yet: applying the resume now would read
    // an empty session and arm a seek that blocks feeding forever. Defer it to `pump_play`.
    let pending = plx_media::route::play_pending();
    let resume = if pending || resume_ns <= 0 {
        plx_media::player::ResumeStart::Settled(plx_media::player::ResumeOutcome::Prepared)
    } else {
        plx_media::player::begin_resume(ps, resume_ns)
    };
    let resume_prepared = matches!(
        resume,
        plx_media::player::ResumeStart::Settled(plx_media::player::ResumeOutcome::Prepared)
    );
    if !resume_prepared && !matches!(resume, plx_media::player::ResumeStart::Pending) {
        if let Some(transaction) = plx_media::route::pending_route_start() {
            let _ = plx_media::route::reject_route_start_preparation(transaction);
        }
    }
    // Enter the player NOW so the HUD draws its Resolving state this frame; `pump_play` starts the
    // engine when the plan lands.
    if pending {
        plx_media::route::arm_play_resume(ps, resume_ns);
        true
    } else if matches!(resume, plx_media::player::ResumeStart::Pending) {
        true
    } else if resume_prepared {
        plx_media::player::start_bufferfeed(ps, pa)
    } else {
        false
    }
}

/// Leaving playback (Stop / BACK / EOS): retire every in-player panel. They are entries on the
/// player page's own `ModalStack`, and unmounting the page unmounts them with it; the dismissal is
/// here because a FAILED playback keeps its `…` popover up over the read-out and BACK must take
/// that panel down first.
pub(crate) fn close_player_overlays(pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>) {
    super::bridge::dismiss_player_overlays(pages);
}
/// **The Info card's tvOS press, committed on the spring-back.** The card's own OK arm cannot
/// perform this: `InfoAction` reaches a detail page or a seek, which needs the route, the trail and
/// the player adapter, and — the reason it is DEFERRED at all — the dip has to be on screen
/// before the card goes away. So the surface arms `PlayerReq::ArmInfoPress`, the loop's press
/// machine holds the frame, and this reads the decision back out of the panel that is still up.
pub(crate) unsafe fn commit_info_press(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) {
    let Some(action) = super::bridge::player_overlay_mut(pages)
        .and_then(|o| o.info_press_action(bridge.metadata_view()))
    else {
        return;
    };
    close_player_overlays(pages);
    apply_info_action(ps, pa, action, pages);
}
/// **Perform the track menu's pick.**
///
/// `route::commit_audio_selection`/`commit_subtitle_selection` take the playback session's `&mut`,
/// which no screen has (§2.2) — the panel returns a
/// [`TrackCommit`](plx_appkit::track_menu::TrackCommit) and this is where it lands, from the
/// overlay's `PlayerReq` and from the headless `plxnative-menupick` trigger alike.
pub(crate) fn commit_track(
    ps: &mut plx_media::route::PlaybackSession,
    commit: plx_appkit::track_menu::TrackCommit,
) {
    use plx_appkit::track_menu::TrackCommit;
    match commit {
        // A local file's track the sink cannot decode has no server to convert it: say so.
        TrackCommit::Audio(audio)
            if plx_media::localfile::is_file_url(&plx_media::route::url(ps))
                && plx_media::player::engine::audio_payload_codec(&audio.codec).is_none() =>
        {
            plx_base::eventlog::log(&format!("local audio: {} is not decodable here; pick refused", audio.codec));
            super::toast::raise(plx_media::localplay::LocalPlayError::AudioCodecs(vec![audio.codec]).message());
        }
        TrackCommit::Audio(audio) => plx_media::route::commit_audio_selection(ps, audio),
        TrackCommit::AudioEnhancement(a) => plx_media::player::request_audio_enhancement(ps, a),
        TrackCommit::Subtitle { render_ordinal, stream_id, sidecar_key, sidecar_codec } => {
            // What the client can draw itself — an embedded ordinal or an external sidecar — is
            // what an Original route (issue #266's candidate) can carry without a burn.
            let client_renderable = render_ordinal >= 0 || sidecar_key.is_some();
            plx_media::route::commit_subtitle_selection(ps, render_ordinal, stream_id, client_renderable);
            // An EXTERNAL pick has no demuxer ordinal (`render_ordinal` is -1, so the embedded
            // renderer is off) — on direct play `player::sidecar` fetches and draws it instead.
            // While transcoding the commit above already asked for a burn and the sidecar draw
            // is silenced for as long as that is true, so selecting here is harmless and means
            // the line survives the playback going BACK to direct play.
            match sidecar_key {
                // A local file's sidecar is a file beside it (`localplay::playing_item`).
                Some(key) if plx_media::localfile::is_file_url(&plx_media::route::url(ps)) => {
                    plx_media::player::sidecar::select_file(stream_id, key, sidecar_codec)
                }
                Some(key) => plx_media::player::sidecar::select(plx_media::route::cur_sid(ps), stream_id, key, sidecar_codec),
                None => plx_media::player::sidecar::deselect(),
            }
        }
        TrackCommit::SubtitleTone(tone) => plx_media::player::set_subtitle_tone(tone),
        // live first, persisted after, nothing republished on completion (`route::select_subtitle_size`)
        TrackCommit::SubtitleSize(size) => plx_media::route::select_subtitle_size(size, None),
        TrackCommit::SubtitlePosition(position) => plx_media::route::select_subtitle_position(position, None),
        TrackCommit::SubtitleOffset(offset) => {
            plx_media::player::set_subtitle_offset(offset);
            plx_media::route::persist_subtitle_offset(ps, offset);
        }
    }
}

pub(crate) fn player_requests(
    repair: &mut plx_media::player::machine::RepairAttempt,
    ps: &mut plx_media::route::PlaybackSession,
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
                retry_failed_playback(ps, pa, bridge.metadata_mut(), None, AttemptOrigin::UserRetry);
            }
            PlayerReq::PlayAutomatically => {
                play_automatically(ps, pa, bridge.metadata_mut());
            }
            PlayerReq::RepairSandbox => {
                if ps.jail_load_blocked {
                    pa.repair_sandbox(repair, plx_platform::tv::sandbox::blocks_native_video());
                    ps.repair_status = repair.state();
                    plx_machine::idle::invalidate();
                }
            }
            PlayerReq::ExtendHud(ms) => {
                if let Some(player) = super::bridge::player_mut(pages) {
                    player.hud.extend(now, ms);
                    player.publish();
                }
            }
            PlayerReq::FocusTabs => {
                if let Some(player) = super::bridge::player_mut(pages) {
                    player.hud.nav.focus = 2;
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
            PlayerReq::More(action) => apply_more_action(ps, pa, bridge, action),
            PlayerReq::CommitTrack(commit) => commit_track(ps, commit),
            PlayerReq::ArmInfoPress => {
                press.begin_ctl(now);
                *ok_armed = true;
            }
            PlayerReq::Info(action) => {
                apply_info_action(ps, pa, action, pages)
            }
            // The old `key_ok`'s tabs-row (`focus == 2`) and failure-read-out (`ChooseQuality`)
            // arms, both of which presented a panel immediately rather than through the deferred
            // press `ArmControlRow`/`ArmInfoPress` use.
            PlayerReq::OpenOverlay(kind) => {
                super::bridge::open_player_overlay(ps, bridge.metadata_view(), pages, kind);
            }
            // The old `key_ok`'s `focus == 1` arm: dip the same tvOS press `ArmInfoPress` does,
            // for the transport's OWN control row rather than a panel's action column. The loop's
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
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
) {
    plx_media::route::cancel_play(ps); // BACK during a load: supersede, drop the landing
    close_player_overlays(pages);
    plx_media::player::stop_bufferfeed(ps, pa);
    // `stop_bufferfeed` reports/clears a real engine through `report::ended`, but a refusal or a
    // BACK during resolve has no engine for teardown to take; retire its in-memory trace here.
    plx_media::player::report::clear_error_trace();
    return_from_player(pages);
}

/// The episode is OVER — drained to EOS, or the user skipped a `final` credits marker.
/// Starts the queued episode when the show has one, else leaves the player exactly as
/// `exit_player` would. There is no interstitial: "always the next episode" — except under the
/// Next episode preference `Off`, which leaves the player here like a film.
///
/// Returns whether playback was handed off to Up Next (`true`) or the player was left (`false`).
/// `exit_player`'s `PopTo` only PARKS the navigation (`Dispatcher::request`, applied at the next
/// commit), so `app.route()` still reads `Player` for the rest of this frame.
pub(crate) fn finish_playback(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) -> bool {
    if plx_media::route::next_episode_mode() != plx_media::route::NextEpisodeMode::Off
        && play_up_next(ps, pa, HUD_LINGER_MS, pages, bridge)
    {
        return true;
    }
    exit_player(ps, pa, pages);
    // The ring goes back to the scrubber for the NEXT session, and the next session is a fresh
    // instance — so there is nothing to park here any more (`start_playback`'s note).
    false
}
/// Activate whatever occupies the control row. ONE dispatch for both the OK key and the
/// pointer — they used to hold byte-identical copies of this `match`, and had already
/// drifted (the key path cleared the held key, the pointer path did not). Returns true when
/// the route flipped, which is the only thing the two callers still handle differently.
#[allow(clippy::too_many_arguments)]
pub(crate) fn activate_ctrl_row(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    slot: plx_appkit::player_hud::ControlSlot,
    btn: c_int,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) -> bool {
    use plx_appkit::player_hud::ControlSlot;
    use plx_appkit::skip_pill::SkipAction;
    match slot {
        // The row's two items, off the cursor the caller already parked (a click sets it
        // from the hit-test, a key press moved it). *Next Episode* starts the successor;
        // *Watch Credits* does nothing beyond the cancel the frame block below performs
        // for it — the button exists so that "let it run" is a THING YOU CAN PRESS rather
        // than an absence, which on a countdown is the difference between choosing and
        // being caught out.
        ControlSlot::UpNext(_) => {
            if btn == plx_appkit::up_next::BTN_NEXT {
                play_up_next(ps, pa, HUD_LINGER_MS, pages, bridge)
            } else {
                if let Some(player) = super::bridge::player_mut(pages) {
                    player.up_next.cancel();
                }
                false
            }
        }
        ControlSlot::Skip(pr) => {
            match pr.action {
                SkipAction::Seek(ns) => {
                    // Retire the segment FIRST: the seek lands on the preceding keyframe, which
                    // is usually still inside it, so without this the button comes straight back
                    // (see `metadata::mark_skipped`).
                    bridge.metadata_mut().run(plx_data::stores::metadata::MetadataCmd::MarkSkipped(pr.marker));
                    request_seek(ns);
                    resume_if_paused(pa);
                    false
                }
                // a `final` credits segment: skipping it IS finishing the item
                SkipAction::Finish => {
                    let _ = finish_playback(ps, pa, pages, bridge);
                    true
                }
            }
        }
        ControlSlot::Discs => false,
    }
}

/// Start the queued episode. Returns false when there is nothing queued (a movie, or the
/// last episode), which is the caller's cue to leave the player.
///
/// It stops the outgoing session ITSELF rather than trusting each call site to: three
/// paths reach here (EOS, Skip Credits on a `final` marker, and OK on the HUD tile while
/// the credits are still rolling) and in all three an Engine is live — `start_bufferfeed`
/// no-ops while one is, so skipping the stop would silently fail to advance. The stop is
/// also what posts the `state=stopped` timeline that commits the watched state, and it
/// must happen BEFORE `request_play_up_next`: teardown reads the outgoing item's session
/// ids and clears the URL, both of which the new plan is about to overwrite.
pub(crate) fn play_up_next(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    hud_ms: u32,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) -> bool {
    // clone off the `&'static` store BEFORE anything can replace it (see `Countdown::take`)
    let Some(u) = super::bridge::player_mut(pages).and_then(|player| player.up_next.take(ps)) else {
        return false;
    };
    // The ratingKey, not the episode title: `rk` is the handle every other line and every harness
    // assertion already uses, and the title is LG's "Content Viewing Information" — the one
    // category this app's Data Safety declaration answers "Not collected" to. `eventlog::scrub` is a
    // backstop for shapes like this; not writing it is the mechanism.
    log(&format!("up next: S{}E{} rk={}", u.season, u.index, u.rk));
    let (rk, resume) = (
        u.rk.clone(),
        plx_data::metadata::resume_ns(u.resume_ms, u.dur_ms),
    );
    close_player_overlays(pages);
    plx_media::player::stop_bufferfeed(ps, pa);
    let ctx = plx_ui::fmt::episode_kicker(u.season, u.index, &u.ep_title);
    if !plx_media::route::request_play_up_next(ps, bridge.metadata_mut(), u, &ctx) {
        return false;
    }
    // Same ritual as `play_item_now`: retire the finished episode's descriptor so the HUD
    // caption and Info card don't label the new playback with the old one's title for the
    // whole pre-roll, and fetch the new leaf off the loop.
    // Read BEFORE `retire_playing` drops the store: the successor is a row of the queue
    // the finished episode created, so it lives on that episode's server.
    let sid = bridge.metadata_view().playing()
        .map(|p| p.sid)
        .unwrap_or_else(plx_plex::plex::current_server);
    bridge.metadata_mut().run(plx_data::stores::metadata::MetadataCmd::RetirePlaying);
    bridge.metadata_mut().run(plx_data::stores::metadata::MetadataCmd::RequestDetail { sid: sid, rk: rk.to_string() });
    start_playback(
        ps,
        pa,
        resume,
        Origin::Unchanged,
        hud_ms,
        None,
        pages,
        bridge,
    );
    true
}

/// **Perform the Info card's chosen action.** The card itself decided and is already dismissing;
/// this is what the loop does about it.
fn apply_info_action(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    action: plx_appkit::info_panel::InfoAction,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
) {
    match action {
        plx_appkit::info_panel::InfoAction::FromBeginning => {
            request_seek(0);
            resume_if_paused(pa);
        }
        // There is no item page to go to: leaving through the exit ritual is the whole action.
        plx_appkit::info_panel::InfoAction::GoToDetail(_) => exit_player(ps, pa, pages),
        plx_appkit::info_panel::InfoAction::None => {}
    }
}

/// Activate the player transport's focused CONTROL ROW item — the deferred half of [`key_ok`]'s
/// player arm, run from the per-frame loop once the press spring-back has played.
///
/// Two arms, in the order they were written in `key_ok`: a STAND-IN owns the row (Skip, Up Next) and
/// performs its own action, or the row holds the three discs and OK opens that disc's panel. `ctrl`
/// is re-resolved by the caller on the committing frame rather than captured at the press, so the
/// activation acts on the row that is DRAWN — the slot is resolved once per loop iteration for input,
/// update and draw alike (see the `let ctrl` at the top of the loop), and an offer that arrived
/// mid-press has already changed what the user is looking at.
#[allow(clippy::too_many_arguments)]
pub(crate) unsafe fn activate_player_row(
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    ctrl: plx_appkit::player_hud::ControlSlot,
    now: u32,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) {
    // The cursor is the instance's; read the one field this dispatch turns on, so the container
    // is borrowed once and the arms below are free to present a panel on it.
    let btn = super::bridge::player(pages).map_or(0, |player| player.hud.nav.btn);
    if !ctrl.is_discs() {
        // A stand-in owns row 1 — activate it. Same value the draw used. (Its `true` answer used
        // to clear the loop's client-side hold-repeat sym as well, so an async route flip could
        // not repeat a held key into the next screen. That timer is gone with phase 10 — see
        // `App::down_sym` — and the discrete lists it drove pace their own `Edge::Repeat`, which
        // a route change ends by retiring the surface that was receiving them.)
        activate_ctrl_row(
            ps,
            pa,
            ctrl,
            btn,
            pages,
            bridge,
        );
    } else if btn == plx_appkit::player_hud::BTN_MORE {
        // …so the discs are what row 1 holds — the complement of the arm above, and the row's only
        // other occupant. OK on a control disc PRESENTS its panel on this page's own stack.
        super::bridge::open_player_overlay(ps, bridge.metadata_view(), pages, plx_screens::player::overlay::OverlayKind::More { quality: false });
    } else {
        super::bridge::open_player_overlay(
            ps,
            bridge.metadata_view(),
            pages,
            plx_screens::player::overlay::OverlayKind::Tracks { tab: if btn == 0 { 1 } else { 0 } },
        );
    }
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
    ps: &mut plx_media::route::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
    now: u32,
    foreground: &mut ForegroundLifecycle,
    repause_at: &mut i64,
    ptr: &mut Pointer,
    pages: &mut plx_ui::dispatch::Dispatcher<super::bridge::AppHost>,
    bridge: &mut super::bridge::Bridge,
) {
    let was_off_player = super::bridge::player(pages).is_none();
    if was_off_player {
        foreground.discard_started_state();
    }
    let activation = drive_foreground(
        foreground,
        ps,
        ForegroundInput::PlayKey,
        &mut PlayerForegroundActuator { pa, repause_at },
    );
    if matches!(activation, ForegroundActivation::Launched) {
        // **A suspended session keeps its ORIGIN, and now it keeps its ENTRY too.** The lifecycle
        // arm used to force the route to Home, which retired the player entry and the page under
        // it; a park moves no page at all, so the only thing left to do here is put the player
        // back on top if something else took it — which, while the tree is parked, nothing does.
        super::bridge::show_page(pages, AppArg::Player);
    } else if matches!(activation, ForegroundActivation::Ordinary) {
        if was_off_player {
            if plx_media::player::start_bufferfeed(ps, pa) {
                // The origin is the page the PLAY key was pressed on — through the same one door
                // `start_playback` uses, so the seed and the push cannot drift apart here.
                enter_player(pages, bridge, Origin::Here, None);
                // Keep the ordinary off-route start's existing stale-Pause defense. A foreground
                // transition applies its explicit clock intent through the lifecycle actuator.
                if paused() {
                    set_transport_paused(pa, false);
                }
            }
        } else if paused() {
            set_transport_paused(pa, false);
        }
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

/// Run a play-plan landing and then observe the derived player state in the same frame. This tiny
/// seam is explicit because a refused `/decision` publishes `Error` inside the landing, after the
/// loop's ordinary report tick; BACK on the next frame can otherwise erase the only observation.
pub(crate) fn land_play_then_observe<S: ?Sized>(
    state: &mut S,
    land: impl FnOnce(&mut S),
    observe: impl FnOnce(&S),
) {
    land(state);
    observe(state);
}

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

#[cfg(test)]
mod play_landing_order_tests {
    use super::land_play_then_observe;
    use std::cell::RefCell;

    #[test]
    fn the_landing_seam_runs_publication_before_observation() {
        let order = RefCell::new(Vec::new());
        // The seam lends a state to both halves (the loop lends it `App`); what is pinned here is
        // the ORDER, so the state is a unit.
        land_play_then_observe(
            &mut (),
            |_| order.borrow_mut().push("landing"),
            |_| order.borrow_mut().push("observation"),
        );
        assert_eq!(*order.borrow(), ["landing", "observation"]);
    }
}

// D8 (UI restructure phase 12): relocated verbatim from `app/mod.rs`'s `delete_local_data_tests`.
// `delete_outcome` (the function under test) stays in `app/input.rs`, beside
// `delete_all_local_data_and_sign_out` — its own caller — which is `input.rs`'s one Settings
// operation that outlives the screen that asked for it; this module is where D8 asks the TEST to
// land, matching that the operation ends a live session exactly as every other exit-of-playback

/// **Where playback returns to**: the `EntryId` of the page that was on top when Play was
/// pressed, seeded at the player's own mount and read back off the mounted screen, over a real
/// container.
#[cfg(test)]
mod player_return_tests {
    use super::super::bridge::{self, AppHost, Bridge};
    use super::{enter_player, return_from_player, Origin};
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
        fn play(&mut self, from: Origin) {
            enter_player(&mut self.d, &mut self.rig, from, None);
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
            p.play(Origin::Here);
            assert!(matches!(p.top(), AppArg::Player), "the player is the page that was pushed");
            p.back_out();
            assert!(p.top().same_instance(&launched_from));
            assert_eq!(p.top_entry(), before, "…and to the SAME entry");
        }
    }

    /// **An auto-advance is not a navigation**: re-entering with the player already on top keeps
    /// the one player and the origin the user came from.
    #[test]
    fn auto_advance_keeps_the_page_the_user_came_from() {
        let _serial = plx_base::testlock::serial();
        let mut p = Pages::new();
        p.stand_on(folder("")).stand_on(folder("/media/shows/s1"));
        let origin = p.top_entry();
        p.play(Origin::Here);
        let player = p.top_entry();
        p.play(Origin::Unchanged);
        assert_eq!(p.top_entry(), player, "no second player was stacked");
        p.back_out();
        assert_eq!(p.top_entry(), origin);
    }

    /// **An origin that is no longer on the stack falls back to the library's root** rather than
    /// to nothing.
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
        assert!(p.top().same_instance(&folder("")), "a stranded return lands on the root");
    }
}

