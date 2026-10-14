//! player::pump — the main-thread pump (was bufferfeed_pump). Runs each frame from
//! plex_run: the pending-seek handler, the ACB-bind state machine (Stage), and the
//! feed dispatch. All ACB/Starfish control calls happen here on the main thread.
use super::engine::{drain_aq, feed_both_lanes};
use super::shared::{HlsPauseCompletion, HlsPrimeKind, HlsSeekPause, Stage};
use super::{ACB_OK, SHARED, TX};
use plx_base::task::MainThread;
use std::os::raw::c_char;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::time::Duration;

/// issue #74 D.1.4: the longest the `loadCompleted` arm will wait for `sf_load` to actually
/// return before giving up and publishing `load_failed`. Kodi's own precedent
/// (`MediaPipelineWebOS.cpp:795-801`) is a 1s `wait_for` then `PLAYER_ABORT`; this budget is
/// deliberately far larger because a real `Load` on the dev set measures in the HUNDREDS OF
/// MILLISECONDS for a cold pipeline (investigation.md §A.0's own citation), and the point of this
/// timeout is to catch a genuine hang — the k5lp `DirectVoInit` block investigation.md §B
/// describes — not to interrupt an ordinary slow one. It exists so a Load that never returns
/// becomes a failure read-out instead of a permanent Connecting spinner.
pub const NATIVE_LOAD_BUDGET: Duration = Duration::from_secs(20);

/// The issued→return gate shared by the constructor wait and the loadCompleted arm.
/// Ready objects retain the existing demux-failure precedence before this gate is evaluated.
fn native_load_gate_ready(epoch: u32) -> bool {
    if SHARED.native_load_returned(epoch) { return true; }
    // Log the deferral exactly once per session (a fast Load may never hit this arm at all,
    // in which case nothing here fires and the ordinary loadCompleted path below runs
    // immediately once the gate is already open).
    if SHARED
        .native_load_deferred_logged_epoch
        .swap(epoch, Relaxed)
        != epoch
    {
        super::log("native: loadCompleted while Load in flight — deferring");
    }
    // D.1.4: bound the wait. A Load that never returns (the k5lp DirectVoInit hang,
    // investigation.md §B) must not leave the player in Connecting forever — past the
    // budget, publish the same signal an outright Load refusal sets, so the EXISTING
    // failure/rollback arm above (this function, near the top) raises the failure read-out
    // on the next tick instead of a permanent spinner.
    match SHARED.native_load_elapsed(epoch) {
        Some(elapsed) if elapsed >= NATIVE_LOAD_BUDGET => {
            super::log(&format!(
                "native: Load did not return within {}s — publishing load_failed",
                NATIVE_LOAD_BUDGET.as_secs()
            ));
            SHARED.load_failed.store(true, Release);
            SHARED.load_timed_out.store(true, Release);
        }
        Some(_) => {}
        // `native_load_elapsed` shares its precondition with `native_load_returned` — both
        // require the epoch to still own `NativeSessionPhase::Active`. A synchronous
        // UnloadCompleted callback (firmware type=23) for this epoch flips the phase to
        // `Unloaded` *without* this arm ever having seen `Returned`, which would otherwise
        // make the budget above unreachable at exactly the moment the gate can never open
        // again — the permanent Connecting spinner D.1.4 exists to rule out. Treat losing
        // `Active` while still deferred as a fired timeout rather than silently disabling it.
        None => {
            super::log(
                "native: Load epoch left Active while loadCompleted was still deferred — \
                 publishing load_failed",
            );
            SHARED.load_failed.store(true, Release);
            SHARED.load_timed_out.store(true, Release);
        }
    }
    false
}

/// Publish the one value the HUD renders from. Pure derivation off signals the workers already
/// maintain — no new cross-thread plumbing, and it runs on every path out of `pump` (including
/// the early returns) so the state can never go stale behind a spinner that never resolves.
/// The playback failed for good: settle the reducer and show the read-out.
fn fail_engine() {
    crate::playback::fail_current_engine();
    set_state(super::shared::PlaybackState::Error);
}

fn set_state(s: super::shared::PlaybackState) {
    SHARED.pb_state.store(s as u8, Relaxed);
}

/// How many seek requests this pump seek MERGED — i.e. requests that arrived while an earlier
/// seek was still resolving and so never got a seek of their own. Reset as it's read, so each
/// logged seek reports only the requests it consumed.
///
/// The count is what makes coalescing observable. `seek_to_ns` only ever holds the newest target,
/// so a burst of six taps and a single tap leave identical state behind; and the burst is applied
/// by whichever of two paths wins a race with the stuck-watchdog, which log different lines.
/// Counting either line therefore measures PMS latency, not merging — `coalesced=` measures
/// merging directly, and both paths report it.
fn take_coalesced() -> u32 {
    TX.seek_reqs.swap(0, Relaxed).saturating_sub(1)
}

/// Place the webOS 5+ exported window: source frame -> on-screen rect.
///
/// No-op on the ACB path. `src` is the frame being FED and `dst` where it lands, and the pair is
/// what expresses scaling — so a 4K direct play must not be described as 1080p, which passing the
/// authoring canvas for both would do. The coded size comes from the demuxer, the only thing that
/// knows it for certain; before it has published, the canvas is the least-wrong fallback and the
/// caller re-places once the real one arrives.
fn place_exported(mt: &MainThread, eng: &mut super::engine::Engine) {
    if super::sink().window_mode() != super::VP_EXPORTED {
        return;
    }
    let (w, h) = SHARED.video_raster();
    let src = if w > 0 && h > 0 { (w, h) } else { (1920, 1080) };
    let rv = unsafe { super::sink().place_window(mt, src.0, src.1, 0, 0, 1920, 1080) };
    eng.placed_src = src;
    super::log(&format!(
        "vplane: exported window placed src={}x{} rv={rv}",
        src.0, src.1
    ));
}

/// Settle a synchronous native reload effect.  A failed `start_bufferfeed` used to be only a log:
/// callers had already advanced the route reducer and then returned, leaving no Engine to publish
/// the failure and (for Original) an `OriginalTrial` which could never receive its first frame.
pub(super) fn settle_reload(outcome:super::engine::ReloadOutcome, operation: &str) -> bool {
    if outcome == super::engine::ReloadOutcome::Started {
        return true;
    }
    super::log(&format!(
        "route transition: {operation} failed synchronously ({outcome:?})",
    ));
    // The reload's teardown parked a Load that had already passed its budget and cleared the
    // session flags with the rest of that session's state, then the start was refused for the
    // outstanding object. Without the cause the report says `unspecified`; the cause is the
    // timeout, so publish it again beside the Error.
    if outcome == super::engine::ReloadOutcome::StartFailed && SHARED.native_load_abandoned() {
        SHARED.load_failed.store(true, Release);
        SHARED.load_timed_out.store(true, Release);
    }
    set_state(super::shared::PlaybackState::Error);
    false
}

/// The per-frame pump: the load gate, Play, in-place seeks, the feed and end-of-stream.
pub fn pump(ps: &mut crate::playback::PlaybackSession, pa: &mut super::adapter::PlayerAdapter, now: u32) {
    use super::shared::PlaybackState;
    // ONE `&mut PlayerAdapter`, split into the live session and the seam token. Everything below
    // that replaces the session (`reload_at`) takes `pa` back, which the borrow checker only
    // allows where neither half is read afterwards.
    let (eng, mt) = pa.split();
    let eng = match eng {
        Some(e) => e,
        None => {
            set_state(PlaybackState::Idle);
            return;
        }
    };
    // The media thread may have returned from sf_load immediately, but it may not settle the start
    // before this Engine exists in the main-thread slot. Drain its exact-attempt result here.
    crate::playback::drain_route_start_results(ps);
    // `sf_load == 0` may leave no callable object, so this must precede the ready wait below;
    // otherwise the pump returns Connecting forever and never consumes the explicit failure.
    if SHARED.load_failed.load(Acquire) {
        fail_engine();
        return;
    }
    // wait for the media-thread ctor
    if unsafe { super::sink().ready(mt) } == 0 {
        if eng.stage == Stage::Loading {
            native_load_gate_ready(eng.native_epoch);
        }
        set_state(PlaybackState::Connecting);
        return;
    }
    if SHARED.frames.load(Relaxed) > 0 {
        crate::playback::confirm_resume_presented(ps);
    }
    // The producer died: the EOS path is gated on `duration_ns > 0` and could never fire, so the
    // player would sit on a black screen forever. Surface it instead — BACK is the escape. A
    // worker may publish a more specific terminal code just before this generic Release flag;
    // Acquire is the matching hand-off.
    if SHARED.demux_io_failed.load(Acquire)
        || (SHARED.demux_failed.load(Acquire) && SHARED.frames.load(Relaxed) == 0)
    {
        fail_engine();
        return;
    }

    // ---------- an audio-track pick: reload at the current position, so the Load names the new
    // track's codec and the demuxer feeds the chosen stream (`player::request_audio_track`). A seek
    // pressed meanwhile supplies the target, so the two become ONE reload. ----------
    if eng.stage >= Stage::Playing && !(eng.flushed && eng.rebase_pending) && TX.audio_reload.swap(false, Relaxed) {
        let pending = TX.seek_to_ns.swap(-1, Relaxed);
        let target = if pending >= 0 { pending } else { SHARED.playpos_ns.load(Relaxed) }.max(0);
        super::log(&format!("audio switch: reload at {}s", target / 1_000_000_000));
        settle_reload(super::engine::reload_at(ps, pa, target), "audio-track reload");
        return; // REPLACED the engine — `eng` dangles.
    }

    // ---------- an in-place seek is still resolving (flushed, not yet re-anchored): COALESCE.
    // Firing another now would race the demux reopen (rapid tap-seeks) and the rebase guard would
    // then eat everything → stuck. Hold the latest requested target (it stays in TX.seek_to_ns) and
    // process it once this seek anchors. If it hasn't anchored within SEEK_STUCK_MS the reopen was
    // lost — first RETRY the reopen cheaply (re-interrupt the demuxer + re-arm), and only fall back
    // to a full (slow) reload if the retries also fail.
    // SEEK_STUCK_MS must comfortably exceed a real reopen+av_seek+first-keyframe on 4K HEVC over
    // PMS (~700ms): the retry closes the demux socket, so a premature watchdog KILLS an open that
    // was about to succeed and restarts it — at 500ms a rapid tap-burst self-DoS'd into the
    // reload fallback every time (caught by the seek_rapid harness cases).
    const SEEK_STUCK_MS: u32 = 1200;
    if eng.flushed && eng.rebase_pending
        && now.wrapping_sub(eng.seek_armed_at) > SEEK_STUCK_MS
    {
        // Adopt the NEWEST coalesced target if later taps landed while this seek was resolving
        // (TX.seek_to_ns holds the latest request): retrying/reloading at the original armed
        // target would land where the user already tapped away from, then seek AGAIN.
        let pending = TX.seek_to_ns.swap(-1, Relaxed);
        let tgt = if pending >= 0 {
            pending
        } else {
            SHARED.seek_target_ns.load(Relaxed)
        }
        .max(0);
        if eng.seek_retries < 2 {
            eng.seek_retries += 1;
            SHARED.seek_to_ns.store(tgt, Release); // re-publish; the demux thread seeks on it
            SHARED.seek_target_ns.store(tgt, Relaxed); // rebase guard keys on the retried target
            eng.seek_armed_at = now;
            super::log(&format!(
                "seek: in-place stuck → retry reopen at {}s (#{}) coalesced={}",
                tgt / 1_000_000_000,
                eng.seek_retries,
                take_coalesced()
            ));
        } else {
            super::log(&format!(
                "seek: in-place stuck → reload at {}s",
                tgt / 1_000_000_000
            ));
            settle_reload(
                super::engine::reload_at(ps, pa, tgt),
                "stuck in-place seek reload",
            ); // REPLACES the engine — eng dangles, return
            return;
        }
    }
    // ---------- pending seek: flush, drop queued AUs, re-point the demux ----------
    let t = TX.seek_to_ns.load(Relaxed);
    if t >= 0
        && !(eng.flushed && eng.rebase_pending) // coalesce: don't stack in-place seeks
        && eng.stage >= Stage::Playing
        && SHARED.duration_ns.load(Relaxed) > 0
        && SHARED.file_size.load(Relaxed) > 0
    {
        TX.seek_to_ns.store(-1, Relaxed);
        let coalesced = take_coalesced(); // read here so BOTH branches consume this seek's requests
        let t = t.max(0);
        // DIRECT-PLAY seek: Kodi IN-PLACE seek — flush + av_seek the demuxer, and
        // (in feed_stream, on the first post-flush keyframe) setTimeToDecode + sendSegmentEvent
        // to re-anchor the GStreamer segment WITHOUT a reload/decoder re-init (no A/V-resync
        // glitch). A plain flush()+refeed with NO fresh segment left a STALE GStreamer segment
        // → the HW sink stops draining ~48 s later → permanent BufferFull + "Playing error"
        // (root-caused by decompiling libpipeline/lxvideosink; see engine::reload_at). Falls
        // back to reload-per-seek if the pipeline isn't reachable (INPLACE_SEEK_OK cleared by
        // feed_stream when sendSegmentEvent can't find it). reload_at REPLACES the ENGINE, so
        // `eng` dangles after it: return immediately.
        if !super::INPLACE_SEEK_OK.load(Relaxed) {
            settle_reload(super::engine::reload_at(ps, pa, t), "direct seek reload");
            return;
        }
        // Pause, user-held seek reuse and eventual prime all share one actuator state. In
        // particular a seek from Paused must not issue a redundant native Pause, and a result from
        // an older transition must not authorize flushing this session's queues.
        match SHARED.prepare_seek_pause() {
            Some(HlsSeekPause::AlreadyHeld) => {}
            Some(HlsSeekPause::Issue(token)) => {
                let accepted = unsafe { super::sink().pause(mt) } != 0;
                match SHARED.complete_seek_pause(token, accepted) {
                    HlsPauseCompletion::Accepted => {}
                    HlsPauseCompletion::Refused => {
                        super::log("seek(in-place): Starfish refused Pause → reload fallback");
                        settle_reload(
                            super::engine::reload_at(ps, pa, t),
                            "seek reload after native Pause refusal",
                        );
                        return;
                    }
                    HlsPauseCompletion::Stale => {
                        super::log("seek(in-place): stale Pause result → reload fallback");
                        settle_reload(
                            super::engine::reload_at(ps, pa, t),
                            "seek reload after stale Pause",
                        );
                        return;
                    }
                }
            }
            None => {
                // The main-thread transition in flight will settle before the next pump tick. Keep
                // the newest seek mailbox intact rather than consuming it into no operation.
                TX.seek_to_ns.store(t, Release);
                TX.seek_reqs.store(1, Release);
                super::log("seek(in-place): clock transition busy; retrying");
                return;
            }
        }
        if !SHARED.begin_native_media_discontinuity(eng.native_epoch) {
            super::log("seek(in-place): native callback epoch retired → reload fallback");
            settle_reload(
                super::engine::reload_at(ps, pa, t),
                "seek reload after retired native epoch",
            );
            return;
        }
        unsafe {
            super::sink().flush(mt); // drop decoded/queued frames only after the clock is really held
        }
        drain_aq(eng);
        // in-place direct-play seek: publish the target and let the DEMUX THREAD av_seek_frame
        // between two reads (ff.rs's inner loop). Nothing here interrupts the socket: the pump
        // used to shutdown(2) it to break the read so the outer loop would reopen+seek, but our
        // AVIO is seekable, so libavformat just healed the broken read through `seek_cb` and read
        // on — no reopen, no seek, and every direct-play seek escalated to a reload. See ff.rs.
        // eng.flushed → feed_stream fires setTimeToDecode+sendSegmentEvent on the first post-seek
        // keyframe. disp_base=0 (rebase to the landed keyframe).
        eng.flushed = true;
        eng.presentation_rearm_pending = false;
        SHARED.seek_to_ns.store(t, Release); // the demux thread's av_seek target
        SHARED.seek_target_ns.store(t, Relaxed); // rebase guard: reject stale drifted keyframes
        SHARED.disp_base.store(0, Relaxed);
        super::log(&format!(
            "seek(in-place): av_seek t={t} coalesced={coalesced}"
        ));
        // zero-base the fed timeline on the first post-seek keyframe (feed_stream), so it
        // presents against the flush-reset clock immediately — no catch-up freeze
        eng.rebase_pending = true;
        eng.rebase_drops = 0;
        eng.seek_armed_at = now; // stuck-watchdog start (see the coalesce/escape above)
        eng.seek_retries = 0;
        eng.max_fed_video_pts = 0;
        eng.max_fed_audio_pts = 0;
        eng.audio_start_fp = None;
        // Drop any AU held from BEFORE the seek (the per-lane BufferFull retry). drain_aq cleared the
        // QUEUES but not these pending AUs; feeding a stale pre-seek AU after the seek re-bases it to
        // the NEW pts_shift → a big A/V desync — the stale audio pts advances max_fed_audio_pts, then
        // the STALE_BACKJUMP guard drops the fresh post-seek audio as "too far back" → no sound. The
        // reopened stream supplies fresh post-seek AUs.
        eng.pending_video = None;
        eng.pending_audio = None;
        eng.prime_play = true; // paused above; Play once PRIME_NS is buffered (no fast-forward)
                               // "no post-seek frame presented yet" — the feed-ahead throttle feeds freely until the first
                               // real presented pts lands, instead of comparing the new fed pts against the STALE pre-seek
                               // presented position (which would wrongly break feeding on a forward in-place seek).
        SHARED.pres_fed.store(super::engine::PRES_NONE, Relaxed);
        eng.feed_pace = super::engine::FeedPace::default(); // the run-on belongs to the pre-seek report
        SHARED.frames.store(0, Relaxed); // count only POST-seek frames (rebind + resume re-pause gate)
                                         // …and forget the last presentation stamp with it. The plane goes dark across a seek by
                                         // design, so the first frame after one would otherwise post a gap the size of the seek and
                                         // read as a stutter (`Shared`'s `dg_vpres_*` block).
        SHARED.dg_vpres_at.store(0, Relaxed);
        SHARED.playpos_ns.store(t, Relaxed); // displayed position jumps; wall clock takes over
    }

    // ---------- load -> Play (decode fed frames as soon as loaded) ----------
    // issue #74 D.1: the gate below requires `SHARED.native_load_returned(eng.native_epoch)`
    // BEFORE this arm may even poll `sf_is_load_completed` — that poll is itself a concurrent
    // Starfish call, and on v0.6.0 it was the FIRST thing this arm did while `sf_load` was still
    // executing (synchronously) on the load thread. See `src/starfish.c`'s `g_load_returned` for
    // the matching seam-side half; `threads::load_thread` flips the Rust side right after the
    // real, blocking `sf_load` call returns. Gated on `!eng.preview_abandon` too, same as the
    // completion arm below: an abandoned trailer-preview Load must not drive any of this — no
    // deferral logging, no budget clock, nothing that reacts to it as if it were live. That is
    // why the guard is checked BEFORE `native_load_gate_ready` is even called: the helper is
    // where the deferral logging and D.1.4 budget/telemetry actually live (see its own doc).
    if eng.stage == Stage::Loading
                && native_load_gate_ready(eng.native_epoch)
        && (SHARED.load_completed.load(Relaxed) || unsafe { super::sink().is_load_completed(mt) } != 0)
    {
        // "native: Load returned after Nms" is logged unconditionally at the gate transition
        // itself (`threads::load_thread`, right where `mark_native_load_returned` flips it), not
        // here — see `load-returned-log-is-conditional-on-loadcompleted-and-unbounded-after`. It
        // used to live in this arm, which meant it only ever appeared paired with the deferral
        // line above, and never at all on the path this fix is most likely to be read on: Load
        // eventually returns but `loadCompleted` goes quiet, so this arm never runs.
        SHARED.load_completed.store(true, Relaxed);
        // when it completed, so the panel can say how long we have been waiting for a frame since
        super::log("SMP loadCompleted");
        eng.stage = Stage::Playing;
        // webOS 5+: place the exported window HERE, the instant Load reports success — which is
        // exactly where ss4s does it (smp_player.c calls StarfishResourcePostLoad synchronously
        // on the line after StarfishMediaAPIs_load returns true).
        //
        // It used to wait for `frames >= 2`, copied from the ACB block below, and that is a
        // DEADLOCK on this path: the pipeline does not emit frames until its sink is bound, and
        // the sink is not placed until frames arrive. The symptom is a player stuck in buffering
        // forever, which is what webOS 6 and 10 reported. The gate is right for ACB and only for
        // ACB — `setMediaVideoData` there needs the sourceInfo envelope, which cannot exist until
        // the pipeline has produced something. The exported window has no such dependency: the
        // BINDING already happened via `option.windowId` in the Load payload, and this call is
        // pure geometry.
        place_exported(mt, eng);
        if super::sink().window_mode() == super::VP_EXPORTED {
            // Playing -> Streaming directly: there is no bind sequence to sit in the middle of.
            // The transition is load-bearing beyond bookkeeping — `pushEOS` is gated on
            // `>= Streaming`, so without it the last frames never drain and Up Next never fires.
            eng.stage = Stage::Streaming;
        }
        // A fresh Load for a seek/resume primes before Play so the clock does not free-run through
        // the demux reopen gap. Segmented HLS needs the same gate even at offset zero: its video
        // and AAC arrive through independent queues, and starting Starfish's audio-master clock
        // before AAC is present produced silent initial playback that recovered only after a seek
        // (the seek path already primed both lanes). Progressive initial play keeps its proven
        // immediate-start behavior.
        if eng.rebase_pending {
            eng.prime_play = true;
            super::log("SMP loadCompleted (priming before Play)");
        } else {
            let generation = SHARED.hls_candidate_generation.load(Acquire);
            let recovery = SHARED.hls_recovery();
            if let Some((token, _)) =
                SHARED.reserve_hls_prime_play(HlsPrimeKind::Fresh, generation, recovery)
            {
                let accepted = unsafe { super::sink().play(mt) } != 0;
                match SHARED.complete_hls_prime_play(token, accepted) {
                    super::shared::HlsPlayCompletion::Accepted { resume_acb } => {
                        if resume_acb {
                            super::acb_mirror_playstate_at(mt, eng.stage, true);
                        }
                        super::log("SMP Play");
                    }
                    super::shared::HlsPlayCompletion::Refused => {
                        eng.prime_play = true;
                        super::log("SMP Play refused; priming for retry");
                    }
                    super::shared::HlsPlayCompletion::Stale => {
                        eng.prime_play = true;
                        super::log("SMP Play result stale; priming for retry");
                    }
                }
            } else {
                eng.prime_play = true;
                super::log("SMP Play fenced; priming before retry");
            }
        }
    } else if eng.stage == Stage::Loading
                && SHARED.native_load_returned(eng.native_epoch)
    {
        // Load HAS returned here (the first arm's `native_load_gate_ready` failed) and
        // `loadCompleted` has NOT arrived (the previous arm's condition also failed) — the other
        // half of issue #74 D.1's budget, and the one `load-returned-log-is-conditional-on-
        // loadcompleted-and-unbounded-after` found unbounded: on v0.6.0 and unchanged until now,
        // a Load that returns ok=1 and then never signals `loadCompleted` left the player parked
        // in Connecting forever, with no counterpart to the D.1.4 timeout above. Gated on
        // `!eng.preview_abandon` too: an abandoned trailer-preview Load has nobody watching a
        // failure read-out for, and must not publish `load_failed`/`load_timed_out` either.
        // Budget it from
        // the moment Load RETURNED, not from when it was ISSUED — `native_load_elapsed` (used
        // above) never resets, so a Load that used most of its own budget getting here would
        // otherwise leave this second wait almost none of its own.
        match SHARED.native_load_returned_elapsed(eng.native_epoch) {
            Some(elapsed) if elapsed >= NATIVE_LOAD_BUDGET => {
                super::log(&format!(
                    "native: loadCompleted did not arrive within {}s after Load returned — \
                     publishing load_failed",
                    NATIVE_LOAD_BUDGET.as_secs()
                ));
                SHARED.load_failed.store(true, Release);
                SHARED.load_timed_out.store(true, Release);
            }
            Some(_) => {}
            // Same rationale as the `None` arm above: losing `Active` (a synchronous
            // UnloadCompleted for this epoch) while still waiting on `loadCompleted` must fire
            // the timeout rather than silently going quiet forever.
            None => {
                super::log(
                    "native: Load epoch left Active while still waiting on loadCompleted after \
                     Load returned — publishing load_failed",
                );
                SHARED.load_failed.store(true, Release);
                SHARED.load_timed_out.store(true, Release);
            }
        }
    }

    // ---------- ACB bind, Kodi/ss4s order: setSinkType(MAIN)+setMediaId+setState(LOADED) ----------
    if eng.stage == Stage::Playing && ACB_OK.load(Relaxed) {
        let id = SHARED.media_id.lock().unwrap().clone();
        if let Some(id) = id {
            unsafe { super::sink().plane_bind(mt, id.as_ptr()) };
            super::log(&format!("SMP ACB bound id={}", id.to_string_lossy()));
            eng.stage = Stage::Bound;
        }
    }

    // ---------- send the WHOLE sourceInfo envelope once frames flow (verbatim, except a
    // container-PQ VP9 stream, see below), then
    // window + PLAYING (setMediaVideoData -> setDisplayWindow -> setState PLAYING) ----------
    if eng.stage == Stage::Bound && !eng.video_info_sent && SHARED.frames.load(Relaxed) >= 2 {
        let bytes = SHARED.source_info.lock().unwrap().clone();
        if let Some(mut bytes) = bytes {
            // A VP9 stream's PQ lives in the container, which the pipeline never sees, so its own
            // envelope says "hdrType":"none". Say HDR10 here when the demuxer found PQ (see
            // `vp9_hdr`). `/tmp/pitv-novp9hdr` sends the envelope as the pipeline made it.
            let container_hdr = *SHARED.container_hdr.lock().unwrap();
            if let Some(h) = container_hdr {
                if !plx_base::devtrig::flag("novp9hdr") {
                    if let Some(rewritten) = super::vp9_hdr::rewrite_source_info(&bytes, &h) {
                        super::log(&super::vp9_hdr::log_line(&h));
                        bytes = rewritten;
                    }
                }
            }
            let rv = unsafe { super::sink().plane_send_video_data(mt, bytes.as_ptr() as *const c_char) };
            super::log(&format!(
                "setMediaVideoData rv={rv} frames={}",
                SHARED.frames.load(Relaxed)
            ));
            if rv != -1 {
                // -1 = client-side isJsonError reject; else accepted
                eng.video_info_sent = true;
                unsafe { super::sink().plane_start(mt, 0, 0, 1920, 1080) };
                eng.stage = Stage::Streaming;
                super::log("setMediaVideoData sent → window+PLAYING");
            }
        }
    }

    // ---------- webOS 5+ (VP_EXPORTED): correct the placement if the real frame size only
    // became known after Load. The demuxer publishes the coded size when it opens the stream,
    // which races the load thread — so the placement above may have used the fallback. Re-placing
    // costs one call, and Kodi re-places on every render-area change anyway.
    // ----------
    if eng.stage >= Stage::Playing && eng.placed_src != (0, 0) {
        let now = SHARED.video_raster();
        if now.0 > 0 && now.1 > 0 && now != eng.placed_src {
            place_exported(mt, eng);
        }
    }

    // ---------- feed AUs after the initial Load has reached Playing. The native clock may itself
    // be Running or Paused: runtime rebuffer and ResumePrime deliberately fill both lanes before
    // issuing Play. NOT while a seek is armed: on a resume the seek is armed before PLAYING, so
    // feeding first would present the file start for a frame before the seek repositions — a
    // visible jump. ----------
    if eng.stage >= Stage::Playing && TX.feed_allowed() && TX.seek_to_ns.load(Relaxed) < 0 {
        // Two-lane feed, then the prime attempt — the ordering and the reason both live in
        // `feed_both_lanes`, so this call site cannot drift from them.
        plx_base::diag::spans::span("feed", || feed_both_lanes(mt, eng));
    }

    // ---------- end-of-stream: the producer hit file EOF (eos_pushed → all AUs to the end were
    // fed) and the pipeline has now played out to within EOS_TAIL of the duration. Mark ended so
    // app.rs tears the player down at the credits instead of freezing on the last frame. Paused
    // at the end stays paused (playpos won't climb) — correct; it fires when resumed. Any seek
    // clears the flag (request_seek), so seeking back from the end doesn't re-trigger. ----------
    // ---------- publish the derived UI state (see PlaybackState's doc) ----------
    // Order matters: a seek in flight outranks "we have frames", because the frames on the panel
    // are the PRE-seek ones and the HUD must show the target, not them.
    set_state(if SHARED.seeking.load(Relaxed) {
        PlaybackState::Seeking
    } else if eng.stage == Stage::Loading {
        PlaybackState::Connecting
    } else if SHARED.frames.load(Relaxed) == 0 {
        PlaybackState::Buffering
    } else {
        PlaybackState::Playing
    });

    const EOS_TAIL_NS: i64 = 1_000_000_000;
    if eng.eos_pushed && !SHARED.ended.load(Relaxed) {
        let dur = SHARED.duration_ns.load(Relaxed);
        let pos = SHARED.playpos_ns.load(Relaxed);
        if dur > 0 && pos >= dur - EOS_TAIL_NS {
            SHARED.ended.store(true, Relaxed);
            super::log(&format!(
                "EOS reached: playpos={}s/{}s → ended",
                pos / 1_000_000_000,
                dur / 1_000_000_000
            ));
        }
    }
}
