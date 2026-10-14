//! player — the buffer-feed video engine (was src/playback.c). THREADING: everything
//! here except sf_on_event/acb_on_event runs on the SDL main thread. Those two are
//! #[no_mangle] and run on the StarfishMediaAPIs library thread; they touch ONLY
//! `SHARED`. Player-engine callback/transport state is synchronized in `shared.rs`; route
//! ownership and route-changing intents have their separate synchronized authority in
//! `route::PLAYER_CONTROL`. The Engine (engine.rs) is main-thread-confined. Design:
//! docs/engine-port-design.md.
//!
//! "Runs on the SDL main thread" is a **compile error to violate** for the two things where it
//! matters — the ACB/Starfish seam and the native session slot. `plex_run` mints ONE
//! [`MainThread`] token; `boot` moves it into [`adapter::PlayerAdapter`], which owns the session
//! that was `engine::ENGINE`. The seam still takes `&MainThread` (reached through the adapter),
//! the slot takes `&mut PlayerAdapter`, and the token is `!Send`, so a closure that captured
//! either cannot be handed to `task::spawn`. The exceptions are the honest ones: the two callbacks
//! above are `extern "C"` entry points *from* the library thread and touch only `SHARED`, and
//! `threads::load_thread` calls `sf_load` off-main by design (see `ffi`).
#![allow(non_upper_case_globals)]
pub mod adapter;
pub mod ass; // pinned libass worker and immutable rendered frames
pub mod ass_script; // a sidecar script fed in windows around the playhead
pub mod ass_source; // bounded embedded scripts and subtitle presentation clock
pub mod engine;
pub mod lifecycle;
pub mod machine;
pub mod ffi_host;
pub mod gst_sink; // the Linux build's player: the fed AUs decoded by the system GStreamer
mod pump;
mod shared;
pub mod sidecar;
pub mod sink; // the verbs the engine speaks to the decoder
pub mod threads;
pub mod video_geometry;
pub mod vp9_hdr; // HDR10 for a VP9 stream: the container's PQ tag, written into the sourceInfo envelope ACB is sent

use plx_base::task::MainThread;
pub use shared::HlsAutomaticTransition;
pub use shared::HlsClockFenceError;
/// one rect of an image-subtitle display set — the demuxer builds them, the HUD draws them
pub use shared::SubRect;
pub use plx_data::metadata::track_names::TrackNames;
pub use shared::UserPauseCursor;
use shared::{
    HlsPauseCompletion, HlsPlayCompletion, HlsUserPause, HlsUserResume, Shared, SubBitmap, SubCue,
    Transport,
};

/// The video sink every seam call goes through. The port installs the television's
/// (`player::ffi::StarfishSink`) or the simulator's (`player::ffi_host::HostSink`) once at boot; a
/// hostsim test binary reads the host sink directly, so it needs no port. Everything in `player/`
/// reaches the seam as `sink().<verb>(mt, ..)`, and nothing branches on which platform it is.
#[cfg(not(any(test, feature = "test-support")))]
fn sink() -> &'static dyn sink::VideoSink {
    &gst_sink::GstSink
}
#[cfg(any(test, feature = "test-support"))]
fn sink() -> &'static dyn sink::VideoSink {
    &ffi_host::HostSink
}

/// `/tmp/pitv-tracknames[=<audio>;<subs>]` — **stand in for the container's own track names**,
/// which nothing off-device can read.
///
/// It exists for the same reason `/tmp/pitv-personbio` does, and the shape of the problem is
/// identical: the data comes from a source no automated or host run can reach, so without a seed
/// every headless look at the screen shows the degenerate state. Here the source is the DEMUXER —
/// `ff::track_names` publishes these when it opens a part — and the desktop simulator has no
/// demuxer at all (the bundled FFmpeg is ARM, and `player::ffi_host` has no video path), so
/// the picker there can only ever draw what PMS sent. Which, for the MP4 this exists to fix, is a
/// column of identical language names.
///
/// Pipe-separated within a list, `;` between the two lists, audio first:
/// `Дубляж|Original;Forced|Full`. Either side may be empty (`;Forced|Full` seeds subtitles alone).
/// An EMPTY file seeds the real nine-track sample this was built against, because that is the shape
/// that exercises the case: six same-language rows PMS reports identically, which no shorter list
/// demonstrates.
///
/// **It seeds the real store and stubs nothing else** — the same `SHARED.track_names` the demuxer
/// writes, read back through the same `metadata::track_label::track_name` precedence. So a seeded
/// screenshot verifies the ROW, honestly; what it cannot verify is the FFI read that fills the
/// store on a television. Compiled out of a release build with every other trigger (`devtrig::read` is
/// a compile-time `None`), so a shipped binary cannot be made to show a name that is not the
/// file's.
pub fn seed_dev_track_names() {
    let Some(spec) = plx_base::devtrig::read("tracknames") else {
        return;
    };
    // The real subtitle names of a nine-track MP4 whose PMS record carries none — the file this
    // whole path was written against. Audio is left empty on purpose: one list is enough to
    // demonstrate, and seeding audio too would hide the `audio_descriptor` fallback the real menu
    // still uses for a track the container does not name.
    const SAMPLE: &str = ";Форс. iTunes|Форс. Jaskier песни|Форс. Red Head Sound песни|Полные iTunes|Полные Jaskier|Полные stirloo|Full|Full SDH|Повнi iTunes";
    let spec = spec.trim().to_string();
    let spec = if spec.is_empty() {
        SAMPLE
    } else {
        spec.as_str()
    };
    let list = |s: &str| -> Vec<String> {
        if s.is_empty() {
            Vec::new()
        } else {
            s.split('|').map(|p| p.trim().to_string()).collect()
        }
    };
    // `split_once(';')`, so a name may not contain a `;` and everything after the first one is the
    // subtitle list — a seed is a diagnostic, not a format, and the alternative is quoting rules.
    let (a, sub) = spec.split_once(';').unwrap_or(("", spec));
    let (audio, subs) = (list(a), list(sub));
    #[cfg(feature = "devtriggers")]
    plx_base::eventlog::log(&format!(
        "player: DEV track names seeded (a={} s={}) — /tmp/pitv-tracknames",
        audio.len(),
        subs.len()
    ));
    *SHARED.track_names.lock().unwrap() = TrackNames { audio, subs };
}
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_long, c_uint};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering::Relaxed};

pub static SHARED: Shared = Shared::new();
pub static TX: Transport = Transport::new();
static ACB_OK: AtomicBool = AtomicBool::new(false); // was the g_acb availability flag

// Kodi in-place seek (flush + reopen + re-anchor the decode position + sendSegmentEvent, NO
// reload/decoder re-init → no HDR-mode popup, no A/V-resync glitch). On webOS<11 (this 4.5)
// setTimeToDecode returns 0, so feed_stream falls back to the content-info path
// (loadSpi_getInfo + setContentInfo(ptsToDecode) — the same path the official app uses).
// Cleared to false if the pipeline can't be reached (sf_send_segment == 0), which drops seeks
// back to the robust reload-per-seek path.
//
// SCOPE: this is a **per-session probe, not a device-capability latch**, and
// `engine::start_bufferfeed` re-arms it to true for every new session. What `sf_send_segment`
// reports is whether `sf_pipeline()` could reach the CustomPipeline behind the CURRENT
// StarfishMediaAPIs object: `SMP_READY()` (dispatch admission set after construction, and cleared
// by destruction or quarantine) plus two non-null shared_ptr hops, `object+0x4c` -> `player+0x04`
// (src/starfish.c). A cleared ready bit does not prove that object storage is unconstructed: the
// safety path retains a quarantined object forever. Every one of these tests is a property of the
// current dispatchable object, not a device capability. `sendSegmentEvent` itself returns
// void, so a 0 here NEVER means "the segment event was rejected", only "there was nothing to
// call it on" — a liveness/timing condition by construction.
// Latching it for the process was therefore a bug with a very long tail: one teardown-window
// race downgraded every later seek of every later item to a ~1 s reload until the app was
// restarted. Re-arming per session is self-healing rather than oscillating, too: the fallback
// the clear selects (`reload_at`) builds a fresh Starfish object, so the exact condition that
// produced the 0 cannot survive into the session that re-arms — and if a fresh session really
// can't reach its pipeline either, it re-clears after one seek and stays on the reload path.
// It remains a static rather than an `Engine` field only because `pump.rs` reads it without an
// Engine borrow at hand; its LIFETIME is the Engine's, since `start_bufferfeed` is the sole
// constructor and the flag is only ever read while a session is live.
pub static INPLACE_SEEK_OK: AtomicBool = AtomicBool::new(true);
static PTYPE: AtomicI32 = AtomicI32::new(10); // g_ptype (PLAYER_TYPE_MSE)

// ---- API app.rs calls (were extern "C" fns in playback.h) ----
//
// `start_bufferfeed`/`start_bufferfeed_tracked` gate on this device's `/dev/rtkmem` jail
// pre-flight (community-tier finding: webosbrew/webos-homebrew-channel PR #202, 2019 Realtek
// k5lp/k3lp sets, default Developer-Mode jailer missing that device node, known to crash native
// A/V apps on this chassis) INSIDE `engine::start_bufferfeed_tracked` itself, latching the
// refusal onto `ps.jail_load_blocked` — a `PlaybackSession` field, not a process-wide static —
// so `state()` derives `PlaybackState::Error` for exactly the attempt that was refused and
// `crate::playback::cancel_play`'s `clear_play_verdict` (the same ritual that retires a `/decision`
// refusal on leaving the player, called from `app.rs`'s `exit_player`) retires it precisely the
// same way. See `engine.rs`'s `start_bufferfeed`/`start_bufferfeed_tracked` for the gate itself.
pub use engine::{
    acb_init, begin_resume, start_bufferfeed, start_bufferfeed_tracked, stop_bufferfeed,
    suspend_bufferfeed, suspend_bufferfeed_if_attempt, BufferfeedStartOutcome, ResumeOutcome,
};

pub use pump::pump;
pub use shared::PlaybackState;

/// The viewer picked an audio track and the stream is being rebuilt for it: `route::
/// commit_audio_selection` calls this once its rebuild is queued. See [`shared::Shared::audio_switch`].
pub fn note_audio_pick() {
    SHARED.audio_pick_ms.store(i64::from(vclock_ms()), Relaxed);
    SHARED.audio_switch.store(shared::AudioSwitch::Picked as u8, Relaxed);
}

/// Is the rebuild now loading the one an audio-track pick asked for? A pick that is still waiting
/// for its reload counts only inside [`shared::AUDIO_PICK_WINDOW_MS`]; once the reload has begun
/// it counts until the session's first frame.
pub fn audio_switching() -> bool {
    match shared::AudioSwitch::from_u8(SHARED.audio_switch.load(Relaxed)) {
        shared::AudioSwitch::Reloading => true,
        shared::AudioSwitch::Picked => {
            i64::from(vclock_ms()) - SHARED.audio_pick_ms.load(Relaxed) <= shared::AUDIO_PICK_WINDOW_MS
        }
        shared::AudioSwitch::None => false,
    }
}

/// [`busy_caption`] with the audio-switch fact given — the pure choice, host-testable.
pub fn busy_caption_with(st: PlaybackState, audio_switch: bool) -> &'static std::ffi::CStr {
    if audio_switch && st == PlaybackState::Buffering {
        plx_platform::i18n::msg::widgets_status_switching_audio_c()
    } else {
        st.caption()
    }
}

pub fn pause(pa: &mut adapter::PlayerAdapter) -> bool {
    match SHARED.prepare_hls_user_pause() {
        Some(HlsUserPause::AlreadyHeld) => {
            TX.commit_paused(true);
            acb_mirror_playstate(pa, false);
            return true;
        }
        Some(HlsUserPause::Issue(token)) => {
            let accepted = unsafe { sink().pause(pa.mt()) } != 0;
            match SHARED.complete_hls_user_pause(token, accepted) {
                HlsPauseCompletion::Accepted => {}
                HlsPauseCompletion::Refused => {
                    log("player: Starfish refused Pause");
                    return false;
                }
                HlsPauseCompletion::Stale => {
                    log("player: Pause result lost its clock token");
                    return false;
                }
            }
        }
        None => {
            log("player: Pause deferred by an in-flight clock transition");
            return false;
        }
    }
    // Publish the feed gate at the same accepted actuator boundary. Leaving this to app.rs after
    // the ACB call let a deadline transaction charge accepted Pause time as active playback.
    TX.commit_paused(true);
    acb_mirror_playstate(pa, false);
    true
} // playback_pause
pub fn resume(pa: &mut adapter::PlayerAdapter) -> bool {
    let queued_stream = pa.engine().is_some();
    match SHARED.prepare_hls_user_resume(queued_stream) {
        Some(HlsUserResume::Deferred) => {
            // Feeding may resume, but an initial/seek/recovery certificate still owns the physical
            // clock. Its eventual Play also carries the pending ACB Resume.
            if TX.seek_preroll_active() {
                TX.finish_seek_preroll();
            }
            TX.commit_paused(false);
            log("player: user Resume accepted; physical Play remains fenced");
            true
        }
        Some(HlsUserResume::Prime) => {
            // Keep Starfish and ACB physically Paused. Opening TX first lets the two AU lanes fill
            // together; their ordinary exact prime certificate owns the eventual Play + ACB
            // Resume. Starting the clock here recreates the pause-to-fill A/V drift race.
            let Some(eng) = pa.engine() else {
                log("player: queued Resume lost its Engine before prime could arm");
                return false;
            };
            engine::arm_live_clock_prime(eng);
            if TX.seek_preroll_active() {
                TX.finish_seek_preroll();
            }
            TX.commit_paused(false);
            log("player: queued Resume feeding; physical Play awaits balanced prime");
            true
        }
        Some(HlsUserResume::Issue(token)) => {
            let accepted = unsafe { sink().play(pa.mt()) } != 0;
            match SHARED.complete_hls_prime_play(token, accepted) {
                HlsPlayCompletion::Accepted { resume_acb } => {
                    if TX.seek_preroll_active() {
                        TX.finish_seek_preroll();
                    }
                    TX.commit_paused(false);
                    if resume_acb {
                        acb_mirror_playstate(pa, true);
                    }
                    true
                }
                HlsPlayCompletion::Refused => {
                    log("player: Starfish refused Play");
                    false
                }
                HlsPlayCompletion::Stale => {
                    log("player: Resume result lost its clock token");
                    false
                }
            }
        }
        None if TX.seek_preroll_active() => {
            // The viewer cancelled "stay paused" after the seek had already transferred the
            // physical hold to Initial/Seek (or after its one-frame Play). No native command is
            // needed here; the prime owns Play if it has not happened yet.
            TX.finish_seek_preroll();
            TX.commit_paused(false);
            true
        }
        None => {
            log("player: Resume has no accepted user hold");
            false
        }
    }
} // playback_resume

pub fn seek_preroll_active() -> bool {
    TX.seek_preroll_active()
}

/// Re-establish the viewer's Pause after the seek prime has decoded its first landed frame. The
/// transport intent stayed Paused throughout; only this method closes the temporary feed override.
pub fn finish_paused_seek(pa: &mut adapter::PlayerAdapter) -> bool {
    if !TX.seek_preroll_active() {
        return true;
    }
    if !pause(pa) {
        return false;
    }
    TX.finish_seek_preroll();
    // One receipt after the accepted pause, never per frame: device scrub checks pair this
    // boundary with stationary playhead samples rather than assuming CommitSeek held the pause.
    log(&format!("seek: paused frame restored ns={}", SHARED.playpos_ns.load(Relaxed)));
    true
}

#[cfg(any(test, feature = "test-support"))]
pub fn force_pause_result_for_test(result: Option<c_int>) {
    ffi_host::force_pause_result_for_test(result);
}

#[cfg(any(test, feature = "test-support"))]
pub fn force_play_result_for_test(result: Option<c_int>) {
    ffi_host::force_play_result_for_test(result);
}

/// Kodi parity: mirror the ACB PLAYSTATE on transport pause/resume (the pipeline Pause/Play alone
/// leaves the app-owned sink's ACB state stale). Only once the plane is streaming — `Bound` means
/// setMediaId/LOADED has happened but setMediaVideoData/window/PLAYING has not, so mirroring a user
/// Resume there would overtake the rest of the ordered bind transaction.
pub fn acb_mirror_playstate(pa: &mut adapter::PlayerAdapter, playing: bool) {
    let (eng, mt) = pa.split();
    let Some(stage) = eng.map(|e| e.stage) else {
        return;
    };
    acb_mirror_playstate_at(mt, stage, playing);
}

/// [`acb_mirror_playstate`] for a caller that already holds the session's `&mut Engine` and so
/// cannot ask the adapter for it a second time — `engine::try_prime`, inside the feed. Splitting
/// the two is what the owned slot forces: with `ENGINE` a `static mut` handing out `&'static mut`,
/// the prime path re-entered the slot it was already holding, which is the aliasing the token
/// could only assert was safe.
pub fn acb_mirror_playstate_at(mt: &MainThread, stage: shared::Stage, playing: bool) {
    if !ACB_OK.load(Relaxed) {
        return;
    }
    if !acb_playstate_ready(stage) {
        return;
    }
    unsafe {
        if playing {
            sink().plane_resume(mt);
        } else {
            sink().plane_pause(mt);
        }
    }
}

fn acb_playstate_ready(stage: shared::Stage) -> bool {
    stage >= shared::Stage::Streaming
}

/// **Answer the Player machine's video-plane question from the seam** (spec §9), once per frame,
/// at §3.3 step 8 — before the opaque-region call and the present gate read the bit.
///
/// `Stage::Streaming` is the FIRST stage at which the ordered ACB bind transaction has finished:
/// `setMediaId` → LOADED → `setMediaVideoData` → `setDisplayWindow` → PLAYING. It is the same
/// predicate [`acb_mirror_playstate`] uses to decide whether the sink can take a PLAYSTATE at all,
/// deliberately — one definition of "the plane is really ours", not two. No engine means no sink,
/// so the bit clears on teardown without teardown having to remember to say so.
///
/// This is an OBSERVATION of the adapter, not a second owner: it hands the answer to
/// [`machine::Player::set_video_plane_bound`], which is the only writer and the only publisher.
pub fn observe_video_plane(
    pl: &mut machine::Player,
    pa: &mut adapter::PlayerAdapter,
) -> Option<bool> {
    let bound = pa.engine().is_some_and(|e| acb_playstate_ready(e.stage));
    pl.set_video_plane_bound(bound)
}

// ---- transport accessors app.rs / player_hud.rs call ----
pub fn is_started() -> bool {
    TX.started.load(Relaxed)
}
/// Coded video raster, atomically published by the demuxer. Subtitle renderers need this
/// independently of the output canvas for anamorphic glyph/blur scaling.
pub fn video_raster() -> (i32, i32) {
    SHARED.video_raster()
}

/// The picture inside the full-screen video window, including non-square pixels.
pub fn video_viewport(width: i32, height: i32) -> video_geometry::Viewport {
    let (w, h) = SHARED.video_raster();
    video_geometry::Aspect::unpack(SHARED.video_aspect.load(std::sync::atomic::Ordering::Acquire))
        .or_else(|| video_geometry::Aspect::from_raster(w, h))
        .unwrap_or_else(|| video_geometry::Aspect::from_raster(width, height).unwrap())
        .fit(width, height)
}

pub fn playpos_ns() -> i64 {
    SHARED.playpos_ns.load(Relaxed)
}
pub fn frames() -> i32 {
    SHARED.frames.load(Relaxed)
}
/// True once this SESSION has presented at least one frame. Deliberately NOT `frames() > 0`: the
/// pump zeroes `frames` as part of applying a seek (`pump.rs`), so that expression reads "no
/// picture" for the whole of every seek. Cleared only by `reset_session` — i.e. by a real stop or
/// a reload, both of which do blank the video plane. See [`shared::Shared::seen_frame`].
pub fn seen_frame() -> bool {
    SHARED.seen_frame.load(Relaxed)
}
pub fn duration_ns() -> i64 {
    SHARED.duration_ns.load(Relaxed)
}
pub fn seek_pending() -> i64 {
    TX.seek_to_ns.load(Relaxed)
}
/// true once the pipeline has drained to true end-of-stream (see pump's EOS check). app.rs polls
/// this to tear the player down at the credits.
pub fn ended() -> bool {
    SHARED.ended.load(Relaxed)
}
pub fn request_seek(ns: i64) {
    SHARED.ended.store(false, Relaxed); // seeking back from the end un-ends the stream
    SHARED.seeking.store(true, Relaxed); // HUD: spinner + freeze the playhead until it lands
    SHARED.seek_display_ns.store(ns, Relaxed);
    TX.seek_to_ns.store(ns, Relaxed);
    // Count the request even though the target it carries may be overwritten before the pump
    // ever sees it — that overwrite IS the coalescing, and this is the only place it's countable.
    TX.seek_reqs.fetch_add(1, Relaxed);
}
/// **The seek was ABANDONED — put the playhead back on reality.**
///
/// [`request_seek`] sets `SHARED.seeking`, and until 2026-08-27 exactly ONE place ever cleared it:
/// the successful prime→Play in `engine::try_prime`. Every path that gives UP on a seek
/// therefore leaked the flag — and that flag is what `pump::set_state` reads to publish
/// `PlaybackState::Seeking`, which means a spinner over the picture, the playhead frozen at
/// `seek_display_ns`, and `is_playing()` false, **for the rest of the playback**, while the
/// pipeline goes on fetching and presenting underneath.
///
/// Device-measured 2026-08-27 (`docs/measurements/j3e-logs/pipe_abr_seek_flat.log`): a transcode
/// seek whose rebuild returned `None` froze the read-out at `pos=5s` while 37 further segments
/// were acquired, four rung commits landed, and the loop held 60 fps for another 84 seconds. The
/// stream was fine; only the app's account of it was stuck.
///
/// A flag set by the requester and cleared only on the success path is a wedge waiting to happen.
/// This exists so every give-up path can say so in one word, rather than each remembering a store
/// — which is the arrangement that failed. `seek_display_ns` goes back to `-1` with it, since the
/// HUD reads that as "no seek target" and a stale one would keep the frozen playhead after the
/// spinner cleared.
pub fn abandon_seek() {
    SHARED.seeking.store(false, Relaxed);
    SHARED.seek_display_ns.store(-1, Relaxed);
    // A failed seek never got the one-frame preroll it was promised. Preserve the viewer's Paused
    // intent and close only the feed override; the existing user clock hold remains authoritative.
    if TX.seek_preroll_active() {
        TX.finish_seek_preroll();
    }
}
/// true while a seek is resolving (request → reopen/reload → prime → Play): the HUD shows a
/// spinner and freezes the playhead at `seek_display_ns` instead of wobbling through the reopen.
pub fn loading(ps: &crate::playback::PlaybackSession) -> bool {
    state(ps).is_busy()
}
/// true only while the pipeline is actually presenting frames — not resolving, connecting,
/// buffering or seeking. app.rs gates the heartbeat's `pos=` field on this: on a **direct-play**
/// resume `begin_resume` only arms the seek (it does not seed `playpos_ns`, unlike the transcode
/// branch), so the position reads 0 until the first decoded frame lands at the resume offset.
/// Logging that pre-roll 0 would show the harness a 0→600 step and read as 600s of "climb"
/// inside one second — a false PASS on `min_timeline_climb_s`.
pub fn is_playing(ps: &crate::playback::PlaybackSession) -> bool {
    matches!(state(ps), shared::PlaybackState::Playing)
}
/// The derived playback state — the ONE thing the HUD renders from. See `PlaybackState`.
pub fn state(ps: &crate::playback::PlaybackSession) -> shared::PlaybackState {
    // A start refused because a timed-out Load is still parked (`ps.native_start_refused`) built
    // no Engine, so the pump that owns `pb_state` never runs and nothing else could say why the
    // player is empty: derive it here, in the one reader.
    if ps.native_start_refused {
        return shared::PlaybackState::Error;
    }
    // A seek in flight is derived HERE too, not only published by the pump's own ladder (which
    // says the same thing in the same order — a seek outranks frames). `request_seek` sets the
    // flag at the press, but `pb_state` is only republished at the end of a pump pass, so a
    // reader between the two still saw Playing: the HUD, which freezes the playhead at the
    // target only while busy, drew one frame of the PRE-seek position between the scrub preview
    // and the frozen target — a visible jump back and forth on every seek.
    if SHARED.seeking.load(Relaxed) {
        return shared::PlaybackState::Seeking;
    }
    shared::PlaybackState::from_u8(SHARED.pb_state.load(Relaxed))
}

/// The one line that makes a photograph of the failure read-out a complete report:
/// `PiTV 0.1.0 · media_source`. Pure: the product, the version and the failure's code, never the
/// file's name.
pub fn support_line(kind: FailureKind) -> String {
    format!("{} {} · {}", plx_base::identity::PRODUCT, plx_base::identity::version(), kind.code())
}

/// **Why a playback failed, as a closed set.** Drives the wording of all three failure surfaces —
/// the HUD caption, the diagnostics panel's verdict line and the full-screen read-out — so they are
/// one decision and can never disagree about what happened.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FailureKind {
    /// The file has no video stream (audio-only, or a container that hides it).
    NoVideoTrack,
    /// The file could not be opened or read as media.
    MediaSource,
    /// Reading stopped part-way through a file that had started.
    PlaybackInterrupted,
    /// The video sink refused the stream (a decoder this machine lacks, a pipeline error).
    TvPipeline,
    /// The sink never answered its Load.
    LoadTimeout,
    Unspecified,
}

impl FailureKind {
    /// The stable code the support line carries.
    pub fn code(self) -> &'static str {
        match self {
            FailureKind::NoVideoTrack => "no_video_track",
            FailureKind::MediaSource => "media_source",
            FailureKind::PlaybackInterrupted => "playback_interrupted",
            FailureKind::TvPipeline => "pipeline",
            FailureKind::LoadTimeout => "load_timeout",
            FailureKind::Unspecified => "unspecified",
        }
    }

    /// A failure a second attempt can plausibly cure (a read error, a slow pipeline), as opposed
    /// to a fact about the file.
    fn transient(self) -> bool {
        !matches!(self, FailureKind::NoVideoTrack)
    }
}

/// What the read-out offers after a failure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FailureAction {
    TryAgain,
    Back,
}

/// What a failure's actions depend on beyond its kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FailureContext {
    /// The failed file is still known, so a retry has something to start.
    pub can_retry: bool,
}

/// The read-out's buttons for `kind`, in order: Try Again when it might help, then Back.
pub fn failure_actions(kind: FailureKind, cx: FailureContext) -> Vec<FailureAction> {
    let mut v = Vec::with_capacity(2);
    if cx.can_retry && kind.transient() {
        v.push(FailureAction::TryAgain);
    }
    v.push(FailureAction::Back);
    v
}

pub fn failure_context(ps: &crate::playback::PlaybackSession) -> FailureContext {
    FailureContext { can_retry: crate::playback::has_url(ps) }
}

/// One failure, worded for each surface.
pub struct ErrorShape {
    pub kind: FailureKind,
    /// The HUD's one-line caption.
    pub caption: &'static std::ffi::CStr,
    /// The diagnostics panel's verdict line.
    pub panel: &'static str,
    /// The full-screen read-out's reason.
    pub readout: &'static str,
    /// A further sentence under the reason, or empty.
    pub detail: std::borrow::Cow<'static, str>,
}

/// What the worker signals say went wrong, most specific first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RuntimeFailure {
    Unknown,
    MediaSource,
    PlaybackInterrupted,
    TvPipeline,
    LoadTimeout,
}

fn runtime_failure(
    demux_failed: bool,
    io_failed: bool,
    load_failed: bool,
    load_timed_out: bool,
) -> RuntimeFailure {
    if load_timed_out {
        RuntimeFailure::LoadTimeout
    } else if load_failed {
        RuntimeFailure::TvPipeline
    } else if io_failed {
        RuntimeFailure::PlaybackInterrupted
    } else if demux_failed {
        RuntimeFailure::MediaSource
    } else {
        RuntimeFailure::Unknown
    }
}

/// The wording for a failure. Pure, so every arm is host-testable.
fn error_shape(no_video: bool, runtime: RuntimeFailure) -> ErrorShape {
    use plx_platform::i18n::msg;
    let shape = |kind, caption, panel, readout| ErrorShape {
        kind,
        caption,
        panel,
        readout,
        detail: std::borrow::Cow::Borrowed(""),
    };
    if no_video {
        return shape(
            FailureKind::NoVideoTrack,
            msg::widgets_failure_no_video_c(),
            msg::widgets_panel_no_video(),
            msg::widgets_reason_no_video(),
        );
    }
    match runtime {
        RuntimeFailure::MediaSource => shape(
            FailureKind::MediaSource,
            msg::widgets_failure_open_c(),
            msg::widgets_panel_open(),
            msg::widgets_reason_open(),
        ),
        RuntimeFailure::PlaybackInterrupted => shape(
            FailureKind::PlaybackInterrupted,
            msg::widgets_failure_stopped_c(),
            msg::widgets_panel_stopped(),
            msg::widgets_reason_stopped(),
        ),
        RuntimeFailure::TvPipeline => shape(
            FailureKind::TvPipeline,
            msg::widgets_failure_tv_rejected_c(),
            msg::widgets_panel_tv_rejected(),
            msg::widgets_reason_tv_rejected(),
        ),
        // A hang, not a refusal: the pipeline never answered, so "rejected" would claim a verdict
        // that was never given.
        RuntimeFailure::LoadTimeout => shape(
            FailureKind::LoadTimeout,
            msg::widgets_failure_load_timeout_c(),
            msg::widgets_panel_load_timeout(),
            msg::widgets_reason_load_timeout(),
        ),
        RuntimeFailure::Unknown => shape(
            FailureKind::Unspecified,
            msg::widgets_status_failed_c(),
            msg::widgets_panel_unknown(),
            msg::widgets_reason_unknown(),
        ),
    }
}

/// The failure now, from the worker signals.
pub fn error_now(ps: &crate::playback::PlaybackSession) -> ErrorShape {
    if let Some(arm) = failtest_arm() {
        return arm;
    }
    error_shape(
        SHARED.demux_no_video.load(Relaxed),
        runtime_failure(
            SHARED.demux_failed.load(std::sync::atomic::Ordering::Acquire),
            SHARED.demux_io_failed.load(std::sync::atomic::Ordering::Acquire),
            SHARED.load_failed.load(Relaxed),
            // The refused start's cause: an abandoned Load exists only after the budget fired.
            SHARED.load_timed_out.load(Relaxed) || ps.native_start_refused,
        ),
    )
}

/// The `failtest` dev trigger: show a chosen failure's wording without having to cause it.
fn failtest_arm() -> Option<ErrorShape> {
    let arm = plx_base::devtrig::read("failtest")?;
    Some(match arm.trim() {
        "novideo" => error_shape(true, RuntimeFailure::Unknown),
        "stream" => error_shape(false, RuntimeFailure::MediaSource),
        "connection" => error_shape(false, RuntimeFailure::PlaybackInterrupted),
        "tv" => error_shape(false, RuntimeFailure::TvPipeline),
        "load_timeout" => error_shape(false, RuntimeFailure::LoadTimeout),
        _ => error_shape(false, RuntimeFailure::Unknown),
    })
}

/// HUD caption for `PlaybackState::Error` (main thread).
pub fn error_caption(ps: &crate::playback::PlaybackSession) -> &'static std::ffi::CStr {
    error_now(ps).caption
}
/// Test-only: drive the derived playback state, returning the previous raw value to restore.
///
/// `pb_state` is the pump's field and `shared` is a private module, so a host test that needs the
/// app in a given state — `app.rs`'s HUD-visibility pair, which pins the bug that made the `…` disc
/// unreachable while stalled — sets it through here rather than widening the module for a test.
/// Callers must hold `plx_base::testlock::serial()`: this is a crate global.
#[cfg(any(test, feature = "test-support"))]
pub fn swap_state_for_test(s: shared::PlaybackState) -> u8 {
    let prev = SHARED.pb_state.load(Relaxed);
    SHARED.pb_state.store(s as u8, Relaxed);
    prev
}
#[cfg(any(test, feature = "test-support"))]
pub fn restore_state_for_test(raw: u8) {
    SHARED.pb_state.store(raw, Relaxed);
}

// ---- diagnostics --------------------------------------------------------------------------

pub use engine::aq_caps;
/// The two feed-ahead throttles, as milliseconds.
///
/// **Not test-only any more, and the reason is the point of N3.** These were `#[cfg(test)]` because
/// only `abr::sim`'s plant read them, to pin half of `B_max` against the pipeline's own values.
/// `abr::plant::b_max_est_ms` now computes the reachable reserve inside the CONTROLLER, from these
/// and `aq_caps()` at run time rather than from a transcription — which is what makes `B*` a
/// property of the plant instead of a number somebody chose. `sim.rs` still keeps its own copy by
/// value, deliberately, so the plant grading the controller is not the controller agreeing with
/// itself.
pub use engine::feed_leads_ms;
pub use sink::{VP_ACB, VP_EXPORTED, VP_NONE};
/// The simulator's clock-sink stop; the television's pipeline has no such control.
pub use ffi_host::stop_clock_at as stop_sim_clock_at;

pub fn seek_display_ns() -> i64 {
    SHARED.seek_display_ns.load(Relaxed)
}
/// The playhead the user INTENDS, which is not always the one being published: while a seek is
/// still resolving (request → reopen → prime → Play) `playpos_ns` keeps reporting the PRE-seek
/// spot, so anything snapshotting "where are we?" inside that window snapshots the position the
/// user just left. The rule — an in-flight seek target wins, else the published position — used to
/// be open-coded at each reader that remembered it and was simply MISSING at the one that did not
/// (the OS-background save; see `app::intended_pos`). This is that rule, once.
///
/// Use it at every reader that means "where the user is". Keep the raw `playpos_ns` only where the
/// PUBLISHED position is the point: the re-pause gate (already behind `seek_pending() < 0`) and the
/// heartbeat's `pos=`, which `tests/run.py` grades real playback progress from — feeding it an
/// intended position would let a seek that never lands read as playback that climbed.
///
/// `appkit/player_hud.rs` deliberately does NOT call this: it needs the same outer two rungs with the
/// live scrub preview between them, so its expression is a superset rather than a caller.
pub fn intended_pos_ns(ps: &crate::playback::PlaybackSession) -> i64 {
    let t = seek_display_ns();
    if loading(ps) && t >= 0 {
        t
    } else {
        playpos_ns()
    }
}
/// Request an audio-track switch: feed the 0-based `audio_idx` audio stream of the same file, with
/// codec `codec`. The pump reloads at the current position on its next tick
/// (`engine::reload_at`), since the Load payload must name the new track's codec.
pub fn request_audio_track(ps: &mut crate::playback::PlaybackSession, audio_idx: i32, codec: &str) {
    stage_native_audio(ps, audio_idx, codec);
    TX.audio_reload.store(true, Relaxed);
}
/// Everything an audio switch needs BEFORE its reload, without queueing one: the Load payload
/// codec, the demuxer's stream index, and the old track's embedded cues gone.
pub fn stage_native_audio(ps: &mut crate::playback::PlaybackSession, audio_idx: i32, codec: &str) {
    crate::playback::set_stream_acodec(ps, codec); // the reload's Load payload uses this audio codec
    SHARED.desired_audio_idx.store(audio_idx, Relaxed);
    SHARED.sub_cues.lock().unwrap().clear();
}
/// reset the subtitle selection to Off — called on a NEW item. Like desired_audio_idx, the
/// subtitle selection PERSISTS across seeks/reloads (it is no longer cleared in reset_session),
/// so a reload-based seek (transcode, or the direct-play reload fallback) keeps the chosen sub
/// instead of silently turning subtitles off.
pub fn reset_subtitle() {
    SHARED.desired_sub_idx.store(-1, Relaxed);
    sidecar::reset(); // …and the previous item's external subtitle file with it
    SUBTITLE_OFFSET_MS.store(0, Relaxed); // …and the timing offset tuned against that file
}

// Dev-only: used only by `route::decision`'s `#[cfg(feature = "devtriggers")]` tests (a store
// build's `CredentialPolicy::HttpsOnly` refuses those tests' plaintext loopback PMS fixtures; see
// the comment on that module's first gated test).

// ---- client-rendered subtitles (direct-play only; a transcode carries no subs) ----
/// selected subtitle track index (-1 = off); the demuxer reads this per block.
pub fn desired_sub_idx() -> i32 {
    SHARED.desired_sub_idx.load(Relaxed)
}
/// select a subtitle track by index (-1 = off). Does NOT clear the cue store: the demuxer
/// pushes cues for EVERY text track regardless of selection, so the buffered region's cues for
/// the newly-selected track are already present and the switch shows immediately. Clearing here
/// would reintroduce the ~10-20s buffer-gap delay (the demuxer runs well ahead of the playhead).
/// A new item / transcode re-point clears the store via reset_session / the pump.
pub fn request_subtitle(idx: i32) {
    SHARED.desired_sub_idx.store(idx, Relaxed);
    if idx < 0 {
        // subs Off: free the image-cue store now (the demuxer also stops decoding new
        // bitmap cues while off — see ff.rs's desired_sub_idx gate)
        SHARED.sub_bitmaps.lock().unwrap().clear();
    }
}

/// The tone client-rendered subtitles are drawn in: an install-wide setting
/// (`plx_data::settings`), read once a frame by the two subtitle draws (`appkit::player_hud`) and by
/// the track menu for its checkmark.
pub fn subtitle_tone() -> plx_data::settings::SubtitleTone {
    plx_data::settings::subtitle_tone()
}

/// The viewer's subtitle timing offset in MILLISECONDS — positive draws every client-rendered cue
/// later, negative earlier. **It belongs to ONE playback of ONE subtitle track**, unlike
/// [`SUBTITLE_TONE`]: a timing error is a property of a track against a media file, so the next
/// film, or another track of this one, must never inherit it. It is remembered per item (never per
/// track) by [`persist_subtitle_offset`], through the memory door, and recalled at that item's next
/// play; [`reset_subtitle`]
/// (a new item) and `route::commit_subtitle_selection` (a different track) put it back to 0. A
/// retry of the same item with the same subtitle carries it ([`restore_subtitle_offset`]).
/// `AtomicI32` rather than `I64`: the range fits trivially, and the 32-bit target gets a plain
/// word load on the per-frame lookups.
static SUBTITLE_OFFSET_MS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// The offset in milliseconds (the unit the menu speaks).
pub fn subtitle_offset_ms() -> i64 {
    i64::from(SUBTITLE_OFFSET_MS.load(Relaxed))
}

/// The offset in nanoseconds (the unit every cue store speaks).
pub(crate) fn subtitle_offset_ns() -> i64 {
    subtitle_offset_ms() * 1_000_000
}

/// **The content time whose cue is on screen at playhead `now_ns`**: the playhead minus the
/// offset, so a +2 s offset shows at 12 s the cue authored for 10 s. Every lookup — embedded text,
/// image sets, the sidecar — goes through this one subtraction; saturating, so no offset can wrap
/// a timestamp at either end of `i64`.
///
/// The sidecar holds its whole file, so any offset in its range is exact there. An EMBEDDED track
/// only has the cues the demuxer has read, which is why [`subtitle_offset_range_ms`] gives it no
/// advance at all. A delay is served from retained cues, but a seek can leave embedded text and
/// image stores without the required history: the demuxer restarts at the target, not before it.
/// Embedded ASS retains known events across an in-place seek, so buffered delayed cues remain
/// available. A full pipeline reload, including a native audio-track switch
/// (`engine::switch_audio_native` → `reload_at`), discards embedded history and can leave a delayed
/// track empty until enough history has been read again.
pub fn subtitle_clock_ns(now_ns: i64) -> i64 {
    now_ns.saturating_sub(subtitle_offset_ns())
}

/// The oldest cue end a store must still hold: the LATEST delay ([`SUBTITLE_OFFSET_LATEST_MS`])
/// plus two seconds behind the playhead, whatever the offset is NOW. A positive offset makes the
/// subtitle clock trail the playhead, and the demuxer never republishes a cue it has read, so a
/// floor that followed the current offset (2 s of history at offset 0) left nothing for a delay
/// raised mid-playback to show until the window had refilled. A negative offset (sidecar only)
/// reads ahead of the playhead, which this floor never prunes. What bounds memory is each store's
/// cap and its eviction order ([`push_subtitle_text`], [`SUB_BITMAP_BUDGET`]), not this floor.
fn subtitle_floor_ns() -> i64 {
    subtitle_floor_for(
        SHARED.playpos_ns.load(Relaxed),
        SHARED.seeking.load(Relaxed),
        SHARED.seek_display_ns.load(Relaxed),
    )
}

fn subtitle_floor_for(position_ns: i64, seeking: bool, target_ns: i64) -> i64 {
    // The demuxer can already be at a backward seek's target while the native clock still
    // reports the old picture. Keep those newly read cues through the rebase. min also makes
    // independently sampled atomics conservative if a new request races this read.
    let anchor = if seeking && target_ns >= 0 { position_ns.min(target_ns) } else { position_ns };
    anchor.saturating_sub((SUBTITLE_OFFSET_LATEST_MS + 2_000) * 1_000_000)
}

/// The latest the offset goes, for every kind of track: a DELAY is served from cues already in the
/// store, which retains this much history at every offset, so a delay raised mid-playback is
/// served at once (`subtitle_floor_ns`, and the image store's eviction order in
/// [`push_subtitle_bitmap`]).
pub const SUBTITLE_OFFSET_LATEST_MS: i64 = 60_000;
/// The earliest a SIDECAR goes. Its whole file is in memory (`sidecar`), so an advance is exactly
/// as servable as a delay.
pub const SUBTITLE_OFFSET_EARLIEST_SIDECAR_MS: i64 = -60_000;
/// The Timing capsule's step (plan `subtitle-menu-capsule` §4), read by `appkit::timing_capsule` on
/// every LEFT/RIGHT `Down`; the Subtitles menu's own Timing row no longer steps anything itself,
/// it only opens the capsule (`appkit::track_menu::TrackOk::OpenTiming`).
pub const SUBTITLE_OFFSET_STEP_MS: i64 = 100;
/// The capsule's step once a held direction has admitted 8 repeats (`appkit::timing_capsule::key`) —
/// a faster walk across the wide sidecar range without losing the 100 ms precision near 0.
pub const SUBTITLE_OFFSET_FAST_STEP_MS: i64 = 500;

/// **The offset range for the selected subtitle, in milliseconds (`(earliest, latest)`)** — the
/// ONE rule the Timing rows (their clamp and their limit dimming, `appkit::track_menu`) and the
/// player's clamp ([`set_subtitle_offset`]) both call, so the menu can never offer a step the
/// player refuses.
///
/// A sidecar gets -60..=+60 s. An EMBEDDED track (text or image) gets 0..=+60 s, a delay only:
/// an advance needs cues the demuxer has not read yet, and an embedded track's packets ride the
/// same byte-bounded A/V queues as the picture (`engine::AQ_VIDEO_BYTES`, 10 MiB — about 2 s of
/// a 40 Mbit/s remux), so an advance would find nothing to draw. Off counts as embedded; the
/// offset is 0 there anyway (a track change resets it).
pub fn subtitle_offset_range_ms() -> (i64, i64) {
    offset_range_for(sidecar::selected())
}

fn offset_range_for(sidecar: bool) -> (i64, i64) {
    let earliest = if sidecar { SUBTITLE_OFFSET_EARLIEST_SIDECAR_MS } else { 0 };
    (earliest, SUBTITLE_OFFSET_LATEST_MS)
}

fn clamp_subtitle_offset_ms(offset_ms: i64) -> i32 {
    let (earliest, latest) = subtitle_offset_range_ms();
    offset_ms.clamp(earliest, latest) as i32
}

/// Select a tone. Takes effect on the next drawn frame — the draws read the setting — so there is
/// nothing to reload and no cue store to touch.
pub fn set_subtitle_tone(tone: plx_data::settings::SubtitleTone) {
    plx_data::settings::set_subtitle_tone(tone);
}

/// Set the timing offset (ms) on the main thread, clamped to the range; like the tone it takes
/// effect on the next drawn frame, with nothing to reload. The live value is not persisted
/// here; a commit remembers it per item, never per track, through the memory door
/// (`route::persist_subtitle_offset`) — see [`SUBTITLE_OFFSET_MS`].
pub fn set_subtitle_offset(offset_ms: i64) {
    SUBTITLE_OFFSET_MS.store(clamp_subtitle_offset_ms(offset_ms), Relaxed);
    plx_machine::idle::invalidate();
}

/// The text store's hard cap, a runaway guard that an ordinary file never reaches. The store
/// holds every text track (the demux pushes them all) from `subtitle_floor_ns` — 32 s behind the
/// playhead at EVERY offset — to the demuxer's read position. The read-ahead is bounded by the
/// A/V queues (`engine::AQ_VIDEO_BYTES` 10 MiB, `AQ_AUDIO_BYTES` 1 MiB); even a 2 Mbit/s encode
/// with 192 kbit/s audio fills them in about 42 s, so the window is at most 32 + 42 = 74 s. A
/// dense dialogue track runs about one cue per second at its peak: 3 text tracks x 74 s x 1
/// cue/s = 222 cues, under half the cap. Eviction is oldest first, i.e. history no delay is
/// reading at an ordinary offset.
const SUB_TEXT_CAP: usize = 512;

/// push a ready (already-clean) subtitle cue into the shared store, tagged with its 0-based
/// track index (the demux pushes for every text track).
/// Bounded by TIME rather than a fixed count: since every track is pushed regardless of
/// selection, drop cues more than the latest delay behind the playhead (`subtitle_floor_ns`)
/// and keep the demuxer's forward window. [`SUB_TEXT_CAP`] guards against a runaway.
pub fn push_subtitle_text(track: i32, start_ns: i64, end_ns: i64, text: String) {
    if text.is_empty() {
        return;
    }
    let mut cues = SHARED.sub_cues.lock().unwrap();
    let floor = subtitle_floor_ns();
    cues.retain(|c| c.end_ns >= floor);
    if cues.len() >= SUB_TEXT_CAP {
        cues.remove(0);
    }
    cues.push(SubCue {
        track,
        start_ns,
        end_ns,
        text,
    });
}
/// demux (D-thread) pushes a subtitle cue (content-time ns) for track `track`. Called for
/// EVERY plain-text track so a mid-play switch is instant; only the selected track's cues are logged.
pub fn push_subtitle_cue(
    track: i32,
    start_ns: i64,
    end_ns: i64,
    payload: &[u8],
) {
    let text = sub_text(payload);
    if text.is_empty() {
        return;
    }
    if track == SHARED.desired_sub_idx.load(Relaxed) {
        // LENGTH, never the dialogue. This line used to carry 34 characters of what the viewer
        // was watching — the most sensitive thing the event log has ever held, in a file that gets
        // photographed into public issue threads. `len=` answers every question the text answered
        // for triage (did a cue arrive, at what time, was it empty, is the track the right one)
        // without being viewing content.
        log(&format!(
            "sub cue [{}..{}ms] len={}",
            start_ns / 1_000_000,
            end_ns / 1_000_000,
            text.chars().count()
        ));
    }
    push_subtitle_text(track, start_ns, end_ns, text);
}
/// the selected track's subtitle text active at `now_ns`, or None (also None when off).
pub fn active_subtitle(now_ns: i64) -> Option<String> {
    active_subtitle_span(now_ns).map(|(text, _, _)| text)
}

/// [`active_subtitle`] with the cue's `(start_ns, end_ns)` in the subtitle clock (the clock
/// [`subtitle_clock_ns`] converts the playhead to).
pub fn active_subtitle_span(now_ns: i64) -> Option<(String, i64, i64)> {
    let sel = SHARED.desired_sub_idx.load(Relaxed);
    if sel < 0 {
        return None;
    }
    let cues = SHARED.sub_cues.lock().unwrap();
    let lookup_ns = subtitle_clock_ns(now_ns);
    cues.iter()
        .rev()
        .find(|c| c.track == sel && lookup_ns >= c.start_ns && lookup_ns < c.end_ns)
        .map(|c| (c.text.clone(), c.start_ns, c.end_ns))
}

/// **The IDENTITY of the text cue active at `now_ns`** — its start, or 0 for none.
///
/// [`active_subtitle`] clones the cue's `String`, which is right for the draw and wrong for the
/// per-frame motion report that has to run whether or not the frame draws (spec §8.3): a subtitle
/// appearing or vanishing is a whole-screen content change with no spring behind it, and before
/// phase 9 nothing reported it because the player route presented unconditionally. `start_ns` is a
/// sufficient identity — two cues of one track cannot share a start.
pub fn subtitle_cue_id(now_ns: i64) -> i64 {
    let sel = SHARED.desired_sub_idx.load(Relaxed);
    if sel < 0 {
        return 0;
    }
    let cues = SHARED.sub_cues.lock().unwrap();
    let lookup_ns = subtitle_clock_ns(now_ns);
    cues.iter()
        .rev()
        .find(|c| c.track == sel && lookup_ns >= c.start_ns && lookup_ns < c.end_ns)
        .map_or(0, |c| c.start_ns)
}

/// **The image-subtitle store's byte ceiling**, which must hold the window a delayed caption
/// needs: from the history floor (`subtitle_floor_ns`: 60 s of the latest delay,
/// `SUBTITLE_OFFSET_LATEST_MS`, + 2 s behind the playhead, retained at EVERY offset so a raised
/// delay finds its sets) to the demuxer's read position.
///
/// - Read-ahead: the demuxer is bounded by the 10 MiB video queue (`engine::AQ_VIDEO_BYTES`) —
///   about 2 s of a 40 Mbit/s remux, 10.5 s of an 8 Mbit/s 1080p encode. Take 12 s: the window
///   is at most 60 + 12 + 2 = 74 s.
/// - Sets in it: a dense dialogue scene publishes about one display set with pixels per 2 s
///   (the CLEAR between two is `close_subtitle_bitmap`, which stores nothing), so 37 sets.
/// - One set: a PGS object is its text's bounding box. Two lines of large text are ~1400x150 px
///   on a 1920x1080 canvas and ~2800x300 px on a 3840x2160 one.
/// - Held as RGBA (4 B/px, how this store kept them first): 37 x 0.84 MB = 31.1 MB at 1080p,
///   and 37 x 3.36 MB = 124.3 MB for a 4K canvas — well over this budget.
/// - Held indexed ([`SubRect`], 1 B/px + a 1 KiB palette): 37 x 0.21 MB = 7.8 MB at 1080p and
///   37 x 0.84 MB = 31.1 MB for a 4K canvas, inside 40 MiB with room for other tracks' sets.
///
/// 40 MiB is 25% of the 160 MB `requiredMemory` the app declares — raised from the 24 MiB this
/// store held at the old 30 s ceiling, which no longer covers a 4K PGS track's indexed worst case
/// above now that the sync-offset ceiling itself is 60 s (`SUBTITLE_OFFSET_LATEST_MS`).
///
/// The floor keeps the window at offset 0 too, where the 60 s of history is only insurance for a
/// delay the viewer has not asked for yet. Under pressure the eviction order drops a set the
/// subtitle clock has passed FIRST ([`push_subtitle_bitmap`]), so at offset 0 that history is
/// best-effort and never costs a set still to be shown; at +60 s nothing has passed the clock, and
/// the arithmetic above is what holds the window.
pub const SUB_BITMAP_BUDGET: usize = 40 * 1024 * 1024;

/// Image-subtitle store (PGS/VobSub). The demux (D) thread decodes EVERY image track while
/// subtitles are on (so a switch between image tracks is instant — see ff.rs) and pushes each
/// display set here; the renderer (M) reads the selected track's active one for the playpos. A
/// new display-set supersedes any still-open cue on the same track (PGS signals the end via a
/// later CLEAR or a superseding set, both handled here). Bounded by time like the text store, and
/// by [`SUB_BITMAP_BUDGET`].
///
/// `cw`/`ch` are the stream's authoring canvas (0 = the decoder never declared one) and every
/// rect's coords are relative to it — the renderer scales the whole set into the video rect, so
/// a 720×480 VobSub and a 1920×1080 PGS land the same size on screen.
pub fn push_subtitle_bitmap(
    track: i32,
    start_ns: i64,
    cw: i32,
    ch: i32,
    rects: Vec<SubRect>,
) {
    if rects.is_empty() {
        return;
    }
    let mut v = SHARED.sub_bitmaps.lock().unwrap();
    for c in v.iter_mut() {
        if c.track == track && c.end_ns == i64::MAX {
            c.end_ns = start_ns; // this set replaces the one still showing
        }
    }
    let floor = subtitle_floor_ns();
    v.retain(|c| c.end_ns >= floor);
    v.push(SubBitmap {
        track,
        start_ns,
        end_ns: i64::MAX,
        cw,
        ch,
        rects,
    });
    // Hard RAM ceiling, by total bytes rather than cue count: several image tracks are buffered
    // at once, and a multi-rect display set counts as the sum of its rects.
    //
    // `v` is in demux (increasing-pts) order and the time-retain above has already dropped
    // everything older than the latest delay's window (`subtitle_floor_ns`). What goes, in order:
    //   1. a set that ENDED before the earlier of the playhead and the subtitle clock, oldest
    //      first — on any track; only a delay raised later could show it again;
    //   2. another track's set, farthest ahead first (a switch to it would at least still find
    //      the cue at the subtitle clock);
    //   3. only then the selected track's farthest set.
    // The selected track's sets ahead of the subtitle clock go last because the demuxer never
    // republishes a set it has read: with a +30 s delay the "far end of the read-ahead" is up to
    // 30 s of captions the viewer has not seen yet, and evicting it (what this did) blanked them.
    // `SUB_BITMAP_BUDGET` is sized so step 3 does not happen over the supported window.
    let mut total: usize = v.iter().map(|c| c.bytes()).sum();
    let now = SHARED.playpos_ns.load(Relaxed);
    let passed = now.min(subtitle_clock_ns(now));
    let sel = SHARED.desired_sub_idx.load(Relaxed);
    while total > SUB_BITMAP_BUDGET && v.len() > 1 {
        let i = v
            .iter()
            .position(|c| c.end_ns <= passed)
            .or_else(|| v.iter().rposition(|c| c.track != sel))
            .unwrap_or(v.len() - 1);
        total -= v[i].bytes();
        v.remove(i);
    }
}
/// A CLEAR display-set (num_rects==0): close the currently-open cue on this track at `end_ns`.
pub fn close_subtitle_bitmap(track: i32, end_ns: i64) {
    let mut v = SHARED.sub_bitmaps.lock().unwrap();
    for c in v.iter_mut() {
        if c.track == track && c.end_ns == i64::MAX {
            c.end_ns = end_ns;
        }
    }
}
/// Cheap per-frame lookup: the `start_ns` key of the selected track's image cue active at
/// `now_ns`, or None. The renderer only re-uploads its GL texture when this key changes.
pub fn active_bitmap_key(now_ns: i64) -> Option<i64> {
    let sel = SHARED.desired_sub_idx.load(Relaxed);
    if sel < 0 {
        return None;
    }
    let v = SHARED.sub_bitmaps.lock().unwrap();
    let lookup_ns = subtitle_clock_ns(now_ns);
    v.iter()
        .rev()
        .find(|c| c.track == sel && lookup_ns >= c.start_ns && lookup_ns < c.end_ns)
        .map(|c| c.start_ns)
}
/// Fetch (canvas_w, canvas_h, rects) for the selected track's display set with this `start_ns`
/// key. Clones the bitmaps once (only when the renderer sees a new key), so the per-frame path
/// stays cheap.
pub fn bitmap_by_key(key: i64) -> Option<(i32, i32, Vec<SubRect>)> {
    let sel = SHARED.desired_sub_idx.load(Relaxed);
    let v = SHARED.sub_bitmaps.lock().unwrap();
    v.iter()
        .rev()
        .find(|c| c.track == sel && c.start_ns == key)
        .map(|c| (c.cw, c.ch, c.rects.clone()))
}
/// Extract plain caption text, stripping markup and normalizing line breaks.
/// ASS/SSA never enters this path: the native renderer receives its complete script/events.
fn sub_text(payload: &[u8]) -> String {
    let s = String::from_utf8_lossy(payload);
    let mut out = String::with_capacity(s.len());
    let mut ch = s.chars().peekable();
    while let Some(c) = ch.next() {
        match c {
            '<' => {
                while let Some(x) = ch.next() {
                    if x == '>' {
                        break;
                    }
                }
            } // <i></i>
            '{' => {
                while let Some(x) = ch.next() {
                    if x == '}' {
                        break;
                    }
                }
            } // {\an8}
            '\\' => match ch.peek() {
                Some('N') | Some('n') => {
                    ch.next();
                    out.push('\n');
                }
                Some('h') => {
                    ch.next();
                    out.push(' ');
                }
                _ => out.push('\\'),
            },
            '\r' => {}
            _ => out.push(c),
        }
    }
    out.trim().to_string()
}

pub use plx_base::eventlog::log; // event-log sink (crate-wide single copy in lib.rs)

fn find(h: &[u8], n: &[u8]) -> bool {
    !n.is_empty() && h.windows(n.len()).any(|w| w == n)
}
/// bytes between `prefix` and the next `term`, or None if `prefix` absent.
fn between(h: &[u8], prefix: &[u8], term: u8) -> Option<Vec<u8>> {
    let start = h.windows(prefix.len()).position(|w| w == prefix)? + prefix.len();
    let rest = &h[start..];
    let end = rest.iter().position(|&b| b == term).unwrap_or(rest.len());
    Some(rest[..end].to_vec())
}

/// Parse one JSON number into thousandths without allocating a JSON tree on the pipeline callback
/// thread. SourceInfo is firmware-owned and may contain integer (`24`) or fractional (`23.976`)
/// frame rates; malformed, zero and non-finite values remain "not reported".
fn source_fps_milli(h: &[u8]) -> Option<i64> {
    let prefix = b"\"frameRate\":";
    let start = h.windows(prefix.len()).position(|w| w == prefix)? + prefix.len();
    let rest = &h[start..];
    let first = rest.iter().position(|b| !b.is_ascii_whitespace())?;
    let number = &rest[first..];
    let end = number
        .iter()
        .position(|b| !b.is_ascii_digit() && !matches!(*b, b'.' | b'-' | b'+'))
        .unwrap_or(number.len());
    let value = std::str::from_utf8(&number[..end])
        .ok()?
        .parse::<f64>()
        .ok()?;
    let milli = value * 1_000.0;
    (value.is_finite() && value > 0.0 && milli <= i64::MAX as f64).then(|| milli.round() as i64)
}

/// Monotonic milliseconds, from an origin fixed at the first call.
///
/// Not SDL ticks: this is read on the pipeline's own callback thread, and the value is only ever
/// differenced, so a private origin is enough and owes SDL nothing.
///
/// **Private to `player::` again.** It was widened to `pub` earlier in phase 9 so
/// `route::decision`'s `auto_last_switch` aging could stamp from it; that stamp now comes from the
/// Player machine's frame tick (`machine::Player::set_now`, spec §4.1), which is the loop's own
/// `fr.now` and therefore cannot disagree with the frame it belongs to. The remaining readers are
/// the pump's silence watchdog and the host clock sink, both inside this module.
fn vclock_ms() -> u32 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_millis() as u32
}

/// **The pipeline's `FRAMEREADY` cadence since the last call**: ticks received, and the worst gap
/// between two consecutive ones in milliseconds. Draining, like
/// [`plx_machine::idle::take_presents`] — the heartbeat is the one caller, once a second.
///
/// **This is a liveness signal, not a frame rate.** See [`sf_on_event`]: the healthy reading on
/// every codec, resolution and container measured so far is `5` and `201`, because the tick is
/// ~5 Hz regardless of the stream's frame rate. What it is good for is the opposite question —
/// whether the pipeline still thinks it is running. A steady `5 / 201` through a picture the
/// viewer says is stuttering is a real and useful finding: it rules the fault OUT of everything
/// this process can see, and sends the search to the display side.
pub fn vplane_take() -> (u32, u32) {
    (
        SHARED.dg_vpres_ct.swap(0, Relaxed),
        SHARED.dg_vpres_gap.swap(0, Relaxed),
    )
}

/// pipeline event on the LIBRARY thread. type 0 = `PF_EVENT_TYPE_FRAMEREADY` (num = fed pts).
///
/// **`FRAMEREADY` is NOT one callback per decoded frame on this firmware, and reading it that way
/// is how a stutter investigation gets the wrong answer.** Kodi's Starfish path treats it as one
/// picture per event, which is where the old "frame presented" gloss here came from. Measured on
/// webOS 4.10.2 (2026-08-21), it is a **~5 Hz position tick**: a 1080p H264 direct play, a 4K HEVC
/// direct play and a visibly stuttering Dolby Vision direct play all deliver it 5 times a second,
/// 201 ms apart, to the millisecond. So [`frames`](crate::player::shared::Shared::frames) counts
/// TICKS, not frames — which is what `pump`'s `frames >= 2` gate really means (≈400 ms of
/// playback, not two pictures) and what `app::diagnostics` really shows.
///
/// The consequence for diagnosis: this callback can say the pipeline still believes it is
/// presenting, and cannot say the picture is smooth. The video plane's real cadence is not
/// observable from this process at all — the evidence for that lives in the TV's own kernel log
/// (`kad-hdr`).
/// Panic-guarded (unwinding into C is UB); touches only SHARED.
#[no_mangle]
pub extern "C" fn sf_on_event(epoch: c_uint, ty: c_int, num: i64, s: *const c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // The mutex is deliberately held through the complete callback. A bare equality check
        // would let teardown retire A, reset the process-long SHARED storage for B, and then let
        // a callback which had already validated A publish into B. Firmware supplies `epoch`
        // through the device-proven callback-context overload of StarfishMediaAPIs::Load.
        let class = match ty {
            0 => shared::NativeEventClass::Presentation,
            23 => shared::NativeEventClass::UnloadCompleted,
            _ => shared::NativeEventClass::Other,
        };
        SHARED.with_native_session(epoch, class, num, || sf_on_event_inner(ty, num, s));
    }));
}

fn sf_on_event_inner(ty: c_int, num: i64, s: *const c_char) {
    if ty != 0 {
        // Latch the FIRST error, with the callback index it arrived at. Sticky, because a later
        // healthy callback must not erase the one event that explains the session — and the index
        // separates "refused immediately" from "died after a long healthy run". Only `ty == 18`:
        // it is the one value this project has ever acted on, and it sits below the 0x1c point
        // where the numbering shifts between webOS 4 and 5+, so it means the same on both. Naming
        // any higher type would be a confident lie on the firmware we cannot test.
        //
        // The firmware's own error NUMBER rides in the same latch (`601` = resource allocation),
        // clamped to the `i32` range: it is the one integer a handled `tv_pipeline` report carries
        // about the refusal. The callback's STRING (`s`, logged below) is never kept: it is free
        // text and can hold a URI or a path.
        if ty == 18 && SHARED.dg_cb_err.load(Relaxed) == 0 {
            SHARED.dg_cb_err.store(ty, Relaxed);
            SHARED
                .dg_cb_err_num
                .store(num.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32, Relaxed);
        }
        let preview = if s.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(s) }
                .to_string_lossy()
                .chars()
                .take(1400)
                .collect()
        };
        log(&format!("smp_cb type={ty} num={num} str={preview}"));
        // **A refusal before the first picture is a VERDICT, not a statistic.** Until 2026-09-03
        // the latch above was all this arm did, and `docs/webos10-lab-report.md` §3.5 records the
        // result on a set that refused the Load asynchronously (`Load()` returned ok=1, then
        // `type=18 num=601 Resource Allocation Error`): `load_failed` stayed false, the pump waited
        // in Connecting for a `loadCompleted` that could never come, the demuxer kept downloading,
        // the adaptive controller stepped down against an estimator that would never see a frame,
        // and the failure read-out — the one screen built to survive a phone photograph — never
        // ran, for ~70 s, on the failure it most exists for. Publishing the same flag the
        // synchronous refusal publishes (`threads::load_thread` on `sf_load == 0`) hands it to the
        // pump's existing `load_failed` arm: HLS rollback if one is pending, else `Error` with
        // `FailureKind::TvPipeline`.
        //
        // Scoped to `!seen_frame` — SESSION-scoped, never `frames == 0`, which a seek zeroes (see
        // player/CLAUDE.md, "`frames` is SEEK-scoped"). A `loadCompleted` already received is NOT
        // an exemption: a completed Load is the pipeline accepting a declaration, and a refusal
        // that lands after it but before any picture is still a session that never started. A
        // type=18 after a picture stays diagnostic-only here: whether that shape exists, and what
        // it means mid-play, is unmeasured, and the lab could not say whether 18 is the only
        // refusal type either — so the `num` is logged rather than filtered on.
        if ty == 18 && !SHARED.seen_frame.load(Relaxed) {
            log(&format!(
                "smp: Load refused by the pipeline before any picture (type=18 num={num}) — failing the source"
            ));
            SHARED.load_failed.store(true, std::sync::atomic::Ordering::Release);
        }
    }
    if ty == 0 {
        // a POSITION UPDATE — map fed pts -> real content position.
        //
        // **It is not one per presented frame, and `vtick`/`vgap` cannot see a dropped frame.**
        // Measured 2026-08-21 across every Profile 5 run: `vtick=5 vgap=201ms`, unvarying, on
        // clean and visibly stuttering playback alike. The pipeline emits this at 5 Hz — it is a
        // position report, not a vsync. This comment used to say "a frame was PRESENTED" and the
        // `dg_vpres_*` block below still describes a cadence probe; both were written from the
        // callback's NAME rather than from its rate, and an instrument that reads a flat 201 ms
        // through the fault it exists to catch is worse than no instrument, because it is quoted.
        //
        // The real per-frame cadence is only observable from LG's own tracing — `GST_DEBUG=
        // dualsequencer:6` via `/tmp/pitv-gstlog`, whose `push_dual` and `lxvideosink`
        // timestamps give one line per frame. That was long avoided as perturbing; it is not, at
        // level 6: the same scene measured 123 LUT misses uninstrumented and 122 with the trace
        // running. Level 9 IS perturbing and is what that reputation came from.
        //
        // `pres_fed` below is still sound — the feed-ahead throttle wants a position, and a 200 ms
        // granularity against a 1.6 s budget is ample. Only the TIMING half was wrong.
        let t = vclock_ms();
        let prev = SHARED.dg_vpres_at.swap(t, Relaxed);
        SHARED.dg_vpres_ct.fetch_add(1, Relaxed);
        if prev != 0 {
            let gap = t.saturating_sub(prev);
            SHARED.dg_vpres_gap.fetch_max(gap, Relaxed);
        }
        SHARED.frames.fetch_add(1, Relaxed);
        SHARED.seen_frame.store(true, Relaxed); // session-scoped: unlike `frames`, a seek won't clear it
        // the rebuild an audio pick asked for has put its picture up: the switch is over
        let _ = SHARED.audio_switch.compare_exchange(
            shared::AudioSwitch::Reloading as u8,
            shared::AudioSwitch::None as u8,
            Relaxed,
            Relaxed,
        );
        SHARED.pres_fed.store(num, Relaxed); // raw fed pts, for the feed-ahead throttle
        SHARED.playpos_ns.store(
            num - SHARED.pts_shift.load(Relaxed) + SHARED.disp_base.load(Relaxed),
            Relaxed,
        );
    }
    if s.is_null() {
        return;
    }
    let b = unsafe { CStr::from_ptr(s) }.to_bytes();

    if let Some(fps_milli) = source_fps_milli(b) {
        SHARED.video_fps_milli.store(fps_milli, Relaxed);
    }
    if find(b, b"\"video\"") {
        if let Some(aspect) = video_geometry::source_aspect(b) {
            SHARED.video_aspect.store(aspect.pack(), std::sync::atomic::Ordering::Release);
        }
    }

    {
        let mut mid = SHARED.media_id.lock().unwrap();
        if mid.is_none() {
            if let Some(id) =
                between(b, b"\"context\":\"", b'"').or_else(|| between(b, b"\"mediaId\":\"", b'"'))
            {
                if let Ok(c) = std::ffi::CString::new(id.clone()) {
                    log(&format!(
                        "SMP context/mediaId={}",
                        String::from_utf8_lossy(&id)
                    ));
                    *mid = Some(c);
                }
            }
        }
    }

    if !SHARED.load_completed.load(Relaxed) && (find(b, b"loadCompleted") || find(b, b"\"loaded\""))
    {
        SHARED.load_completed.store(true, Relaxed);
        log("SMP loadCompleted");
    }

    {
        // capture the WHOLE sourceInfo envelope VERBATIM (byte-for-byte + NUL), never re-encoded
        let mut si = SHARED.source_info.lock().unwrap();
        if si.is_none() && find(b, b"\"video\":") && find(b, b"\"context\":") {
            let mut v = Vec::with_capacity(b.len() + 1);
            v.extend_from_slice(b);
            v.push(0);
            log(&format!("SMP sourceInfoRaw captured ({} bytes)", b.len()));
            *si = Some(v);
        }
    }
}

#[no_mangle]
pub extern "C" fn acb_on_event(ev: c_long, reply: *const c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let r = if reply.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(reply) }
                .to_string_lossy()
                .into_owned()
        };
        log(&format!("acb_cb ev={ev} reply={r}"));
    }));
}

/// Every kind × every context the table can see, so a rule is a property, not an example. Shared
/// with the tests outside this module that grade the table against a screen's own limits.
#[cfg(any(test, feature = "test-support"))]
pub fn every_failure_row() -> Vec<(FailureKind, FailureContext, Vec<FailureAction>)> {
    use FailureKind as K;
    let kinds = [K::NoVideoTrack, K::MediaSource, K::PlaybackInterrupted, K::TvPipeline, K::LoadTimeout, K::Unspecified];
    let mut out = Vec::new();
    for kind in kinds {
        for can_retry in [false, true] {
            let cx = FailureContext { can_retry };
            out.push((kind, cx, failure_actions(kind, cx)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // The jail/rtkmem refusal (no Engine ever built, `state()` deriving `Error`, and the
    // refusal retiring on route exit) is exercised by
    // `native_failure_regressions::jail_refusal_enters_error_without_engine_and_retires_on_exit`
    // below, against the real `ps.jail_load_blocked`-based gate in `engine.rs` — see that test.

    #[test]
    fn acb_pause_resume_cannot_overtake_the_bind_transaction() {
        assert!(!acb_playstate_ready(shared::Stage::Playing));
        assert!(!acb_playstate_ready(shared::Stage::Bound));
        assert!(acb_playstate_ready(shared::Stage::Streaming));
    }

    #[test]
    fn source_info_reports_the_stream_fps_not_the_position_tick_rate() {
        assert_eq!(
            source_fps_milli(br#"{"video":{"frameRate":24,"width":3840}}"#),
            Some(24_000),
        );
        assert_eq!(
            source_fps_milli(br#"{"video":{"frameRate": 23.976,"width":1920}}"#),
            Some(23_976),
        );
        assert_eq!(source_fps_milli(br#"{"video":{"frameRate":0}}"#), None);
        assert_eq!(source_fps_milli(br#"{"video":{"width":1920}}"#), None);
    }

    #[test]
    fn callback_after_native_session_retirement_cannot_mutate_the_idle_session() {
        let _guard = plx_base::testlock::serial();
        SHARED.reset_session();

        sf_on_event(1, 0, 7_000_000_000, std::ptr::null());
        let mutated = SHARED.seen_frame.load(std::sync::atomic::Ordering::Acquire)
            || SHARED.frames.load(std::sync::atomic::Ordering::Acquire) != 0
            || SHARED.pres_fed.load(std::sync::atomic::Ordering::Acquire) != 0
            || SHARED.playpos_ns.load(std::sync::atomic::Ordering::Acquire) != 0;

        SHARED.reset_session();
        assert!(
            !mutated,
            "a callback with no live native-session owner must be discarded"
        );
    }

    /// The four shapes a `type=18` can arrive in, graded on the ONE bit that decides them:
    /// `seen_frame`. Before any picture it is a refusal whether or not `loadCompleted` came first;
    /// after a picture it is diagnostic only — including after a seek, which zeroes `frames` but
    /// never `seen_frame` (the trap player/CLAUDE.md names).
    #[test]
    fn a_type_18_before_any_picture_publishes_load_failed_and_after_one_does_not() {
        let _guard = plx_base::testlock::serial();
        let refusal = c"Resource Allocation Error".as_ptr();
        let refuse = |epoch: u32| sf_on_event(epoch, 18, 601, refusal);
        let failed = || SHARED.load_failed.load(std::sync::atomic::Ordering::Acquire);

        // 1. before loadCompleted
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("session");
        refuse(epoch);
        assert!(failed(), "a refusal before loadCompleted is a verdict");
        SHARED.retire_native_session(epoch);

        // 2. after loadCompleted, still no picture
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("session");
        sf_on_event(epoch, 2, 0, c"{\"loadCompleted\":true}".as_ptr());
        assert!(SHARED.load_completed.load(std::sync::atomic::Ordering::Acquire));
        refuse(epoch);
        assert!(
            failed(),
            "a completed Load is a declaration accepted, not a picture; the refusal still counts"
        );
        SHARED.retire_native_session(epoch);

        // 3. after a picture: diagnostic only
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("session");
        sf_on_event(epoch, 0, 7_000_000_000, std::ptr::null());
        assert!(SHARED.seen_frame.load(std::sync::atomic::Ordering::Acquire));
        refuse(epoch);
        assert!(!failed(), "a type=18 on an established session is not a Load refusal");
        assert_eq!(
            SHARED.dg_cb_err.load(std::sync::atomic::Ordering::Acquire),
            18,
            "…but the diagnostics latch still records it"
        );

        // 4. after a seek on that session: `frames` is zeroed, `seen_frame` is not
        SHARED.frames.store(0, std::sync::atomic::Ordering::Relaxed);
        refuse(epoch);
        assert!(
            !failed(),
            "a seek zeroes `frames`; the refusal predicate must read `seen_frame`, not that"
        );
        SHARED.retire_native_session(epoch);
        SHARED.reset_session();
    }

    /// **The firmware's error number is latched beside the type, once.** `type=18 num=601
    /// Resource Allocation Error` is the shape the lab recorded; the `601` is what a handled
    /// `tv_pipeline` report carries, so it must survive a later healthy callback and a later,
    /// different error, and must go with the rest of the session at `reset_session`.
    #[test]
    fn the_first_type_18_latches_its_number_and_nothing_later_erases_or_replaces_it() {
        let _guard = plx_base::testlock::serial();
        let num = || SHARED.dg_cb_err_num.load(std::sync::atomic::Ordering::Acquire);
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("session");
        assert_eq!(num(), 0, "nothing recorded yet");
        sf_on_event(epoch, 2, 0, c"{\"loadCompleted\":true}".as_ptr());
        assert_eq!(num(), 0, "a healthy callback is not an error");
        sf_on_event(epoch, 18, 601, c"Resource Allocation Error".as_ptr());
        assert_eq!(num(), 601);
        sf_on_event(epoch, 2, 0, c"{}".as_ptr());
        sf_on_event(epoch, 0, 7_000_000_000, std::ptr::null());
        assert_eq!(num(), 601, "a later healthy callback does not erase the latch");
        sf_on_event(epoch, 18, 99, std::ptr::null());
        assert_eq!(num(), 601, "the FIRST error is the one that explains the session");
        assert_eq!(SHARED.dg_cb_err.load(std::sync::atomic::Ordering::Acquire), 18);
        SHARED.retire_native_session(epoch);
        SHARED.reset_session();
        assert_eq!(num(), 0, "cleared where `dg_cb_err` is cleared");
    }

    /// The number is bounded to the `i32` range on the way in: the callback hands over an `i64`,
    /// and the report carries only a value a fixed-width integer field can hold.
    #[test]
    fn a_type_18_number_outside_the_i32_range_is_clamped_not_wrapped() {
        let _guard = plx_base::testlock::serial();
        let num = || SHARED.dg_cb_err_num.load(std::sync::atomic::Ordering::Acquire);
        for (given, want) in [
            (i64::MAX, i32::MAX),
            (i64::from(i32::MAX) + 1, i32::MAX),
            (i64::MIN, i32::MIN),
            (-5, -5),
        ] {
            SHARED.reset_session();
            let epoch = SHARED.begin_native_session().expect("session");
            sf_on_event(epoch, 18, given, std::ptr::null());
            assert_eq!(num(), want, "num {given}");
            SHARED.retire_native_session(epoch);
        }
        SHARED.reset_session();
    }

    /// A SYNCHRONOUS Load refusal (`sf_load` returning 0) has no firmware number: Starfish's `Load`
    /// answers with a bare success flag, and the other refusals inside `sf_load` are the seam's own
    /// lifecycle gates. So it publishes `load_failed` and leaves the latch at 0 — pinned so a
    /// number is never invented for it.
    #[test]
    fn a_synchronous_load_refusal_publishes_no_firmware_number() {
        let _guard = plx_base::testlock::serial();
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("session");
        assert!(SHARED.publish_native_load_failure(epoch));
        assert!(SHARED.load_failed.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(SHARED.dg_cb_err_num.load(std::sync::atomic::Ordering::Acquire), 0);
        SHARED.retire_native_session(epoch);
        SHARED.reset_session();
    }

    #[test]
    fn late_native_callback_cannot_cross_into_the_next_session() {
        let _guard = plx_base::testlock::serial();
        SHARED.reset_session();
        let retired = SHARED.begin_native_session().expect("session A");
        assert!(SHARED.retire_native_session(retired));
        let current = SHARED.begin_native_session().expect("session B");

        sf_on_event(retired, 0, 7_000_000_000, std::ptr::null());
        let stale_mutated = SHARED.seen_frame.load(std::sync::atomic::Ordering::Acquire)
            || SHARED.frames.load(std::sync::atomic::Ordering::Acquire) != 0
            || SHARED.pres_fed.load(std::sync::atomic::Ordering::Acquire) != 0
            || SHARED.playpos_ns.load(std::sync::atomic::Ordering::Acquire) != 0;
        sf_on_event(current, 0, 8_000_000_000, std::ptr::null());
        let current_landed = SHARED.seen_frame.load(std::sync::atomic::Ordering::Acquire)
            && SHARED.pres_fed.load(std::sync::atomic::Ordering::Acquire) == 8_000_000_000;

        SHARED.retire_native_session(current);
        SHARED.reset_session();
        assert!(!stale_mutated, "session A must not mutate session B");
        assert!(current_landed, "session B's own callback must still land");
    }

    #[test]
    fn native_epoch_retirement_drains_a_callback_already_inside_the_reducer() {
        let _guard = plx_base::testlock::serial();
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("native session");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let callback = std::thread::spawn(move || {
            SHARED.with_native_session(epoch, shared::NativeEventClass::Other, 0, || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("callback entered the native-session reducer");

        let (retired_tx, retired_rx) = std::sync::mpsc::channel();
        let retire = std::thread::spawn(move || {
            retired_tx
                .send(SHARED.retire_native_session(epoch))
                .unwrap();
        });
        assert!(
            retired_rx
                .recv_timeout(std::time::Duration::from_millis(20))
                .is_err(),
            "retirement crossed a callback which had already been admitted",
        );
        release_tx.send(()).unwrap();
        assert!(retired_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("retirement completes after the callback leaves"),);
        callback.join().unwrap();
        retire.join().unwrap();
        SHARED.reset_session();
    }

    #[test]
    fn unload_completed_is_an_explicit_terminal_native_session_transition() {
        let _guard = plx_base::testlock::serial();
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("native session");

        sf_on_event(epoch, 23, 0, std::ptr::null());
        assert!(SHARED.native_unload_completed(epoch));
        sf_on_event(epoch, 0, 8_000_000_000, std::ptr::null());
        assert_eq!(
            SHARED.frames.load(std::sync::atomic::Ordering::Acquire),
            0,
            "an event after unload-completed entered the terminal Rust epoch",
        );
        assert!(SHARED.retire_native_session(epoch));
        let next_epoch = SHARED
            .begin_native_session()
            .expect("the next Load can mint an epoch after native gate+Rust retirement");
        assert_ne!(next_epoch, epoch);
        assert!(
            SHARED.begin_native_session().is_none(),
            "overlapping native Load was admitted",
        );
        assert!(SHARED.retire_native_session(next_epoch));
        SHARED.reset_session();
    }

    #[test]
    fn pre_seek_presentation_cannot_certify_the_post_seek_timeline() {
        let _guard = plx_base::testlock::serial();
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("native session");
        assert!(SHARED.begin_native_media_discontinuity(epoch));

        sf_on_event(epoch, 0, 7_000_000_000, std::ptr::null());
        assert_eq!(SHARED.frames.load(std::sync::atomic::Ordering::Acquire), 0);
        assert_eq!(
            SHARED.playpos_ns.load(std::sync::atomic::Ordering::Acquire),
            0
        );
        assert!(
            !SHARED.seen_frame.load(std::sync::atomic::Ordering::Acquire),
            "an old type-0 callback cannot prove the new seek presented"
        );

        assert!(SHARED.arm_native_presentations(epoch));
        sf_on_event(epoch, 0, 8_000_000_000, std::ptr::null());
        assert_eq!(SHARED.frames.load(std::sync::atomic::Ordering::Acquire), 1);
        assert_eq!(
            SHARED.pres_fed.load(std::sync::atomic::Ordering::Acquire),
            8_000_000_000,
        );

        SHARED.retire_native_session(epoch);
        SHARED.reset_session();
    }

    #[test]
    fn post_seek_feed_commits_or_discards_callbacks_that_race_its_reply() {
        let _guard = plx_base::testlock::serial();
        SHARED.reset_session();
        let epoch = SHARED.begin_native_session().expect("native session");
        assert!(SHARED.begin_native_media_discontinuity(epoch));

        // A BufferFull/error Feed may race a position callback, but the AU was not accepted. The
        // callback is latched during the call and discarded with its failed transaction.
        assert!(SHARED.begin_native_presentation_probe(epoch));
        sf_on_event(epoch, 0, 7_000_000_000, std::ptr::null());
        assert_eq!(SHARED.frames.load(std::sync::atomic::Ordering::Acquire), 0);
        assert!(SHARED.reject_native_presentation_probe(epoch));
        sf_on_event(epoch, 0, 7_500_000_000, std::ptr::null());
        assert_eq!(SHARED.frames.load(std::sync::atomic::Ordering::Acquire), 0);

        // On the retained AU's accepted retry, a callback which arrives before Feed returns is
        // replayed exactly once at commit and later callbacks flow normally.
        assert!(SHARED.begin_native_presentation_probe(epoch));
        sf_on_event(epoch, 0, 8_000_000_000, std::ptr::null());
        assert_eq!(SHARED.frames.load(std::sync::atomic::Ordering::Acquire), 0);
        assert!(SHARED.commit_native_presentation_probe(epoch, |num| {
            sf_on_event_inner(0, num, std::ptr::null())
        }));
        assert_eq!(SHARED.frames.load(std::sync::atomic::Ordering::Acquire), 1);
        assert_eq!(
            SHARED.pres_fed.load(std::sync::atomic::Ordering::Acquire),
            8_000_000_000
        );
        sf_on_event(epoch, 0, 8_200_000_000, std::ptr::null());
        assert_eq!(SHARED.frames.load(std::sync::atomic::Ordering::Acquire), 2);

        assert!(SHARED.retire_native_session(epoch));
        SHARED.reset_session();
    }

    /// User Play releases the transport pause, but an internal runway hold owns the media clock.
    /// Calling the ordinary seam here would bypass the only place that checks fresh A/V media and
    /// recreate the short burst/freeze cycle after a manual Pause/Play during rebuffering.
    #[test]
    fn user_resume_cannot_bypass_an_internal_hls_rebuffer_hold() {
        let _guard = plx_base::testlock::serial();
        let old_paused = TX.paused.load(std::sync::atomic::Ordering::Acquire);
        SHARED.reset_hls_clock_for_test();
        let pause = SHARED
            .prepare_hls_rebuffer_pause()
            .expect("reserve internal Pause");
        assert_eq!(
            SHARED.complete_hls_rebuffer_pause(pause, true),
            HlsPauseCompletion::Accepted
        );
        assert_eq!(
            SHARED.prepare_hls_user_pause(),
            Some(HlsUserPause::AlreadyHeld)
        );
        TX.commit_paused(true);
        struct Restore(bool);
        impl Drop for Restore {
            fn drop(&mut self) {
                SHARED.reset_hls_clock_for_test();
                TX.commit_paused(self.0);
            }
        }
        let _restore = Restore(old_paused);
        let before = ffi_host::play_calls_for_test();
        let mut pa = adapter::PlayerAdapter::new(unsafe { plx_base::task::MainThread::assume() });

        assert!(resume(&mut pa));

        assert_eq!(
            ffi_host::play_calls_for_test(),
            before,
            "ordinary Resume called Starfish while the measured-runway gate still owned the clock",
        );
        assert!(SHARED
            .hls_rebuffering
            .load(std::sync::atomic::Ordering::Acquire));
        assert!(!TX.paused.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn an_abandoned_paused_seek_keeps_user_pause_and_closes_only_its_feed_override() {
        let _guard = plx_base::testlock::serial();
        TX.reset();
        TX.commit_paused(true);
        TX.resume_pend
            .store(true, std::sync::atomic::Ordering::Release);
        assert!(TX.begin_paused_seek());
        // Test setup only: avoid looking like a second production arm site to the source-level
        // invariant in tests/test_harness.py.
        SHARED
            .seeking
            .swap(true, std::sync::atomic::Ordering::AcqRel);

        abandon_seek();

        assert!(TX.paused.load(std::sync::atomic::Ordering::Acquire));
        assert!(!TX.seek_preroll_active());
        assert!(!TX.resume_pend.load(std::sync::atomic::Ordering::Acquire));
        TX.reset();
    }

    /// **A seek that is given up on must not leave the spinner armed forever.**
    ///
    /// `request_seek` sets `SHARED.seeking`, and the ONLY place that cleared it was the successful
    /// prime→Play. `pump::set_state` publishes `PlaybackState::Seeking` from that flag ahead of
    /// every other arm — deliberately, since the frames on the panel during a seek are the
    /// pre-seek ones — so a leaked flag means a permanent spinner, a playhead frozen at the target
    /// and `is_playing()` false, while the pipeline plays on underneath. Device-measured: 84
    /// seconds of exactly that, through 37 segment acquisitions and four rung commits.
    ///
    /// Differential: against the code before `abandon_seek` existed there was nothing to call, and
    /// both assertions below fail on the state `request_seek` leaves.
    ///
    /// Takes the crate lock because `SHARED` is a process-wide global and other modules' tests
    /// read the playback state.
    #[test]
    fn an_abandoned_seek_disarms_the_spinner_and_the_frozen_playhead() {
        let _guard = plx_base::testlock::serial();
        let was_seeking = SHARED.seeking.load(Relaxed);
        let was_display = SHARED.seek_display_ns.load(Relaxed);

        request_seek(40_000_000_000);
        assert!(
            SHARED.seeking.load(Relaxed),
            "the requester arms the spinner"
        );
        assert_eq!(
            seek_display_ns(),
            40_000_000_000,
            "and freezes the playhead at the target"
        );

        abandon_seek();
        assert!(!SHARED.seeking.load(Relaxed), "giving up must disarm it");
        assert_eq!(
            seek_display_ns(),
            -1,
            "a stale target would keep the playhead frozen"
        );

        SHARED.seeking.store(was_seeking, Relaxed);
        SHARED.seek_display_ns.store(was_display, Relaxed);
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> SubRect {
        SubRect {
            x,
            y,
            w,
            h,
            index: vec![0u8; (w * h) as usize],
            palette: Box::new([[0u8; 4]; 256]),
        }
    }

    /// A rect whose share of the store's byte budget is about `bytes`, whatever a rect's pixels
    /// cost in the store's representation (measured, not assumed).
    fn rect_of(bytes: usize) -> SubRect {
        let set = |h| SubBitmap { track: 0, start_ns: 0, end_ns: 0, cw: 0, ch: 0, rects: vec![rect(0, 0, 1024, h)] };
        let per_row = set(2).bytes() - set(1).bytes();
        rect(0, 0, 1024, (bytes / per_row) as i32)
    }

    /// One second, in the cue stores' unit. The offset is set in MILLISECONDS and every cue is in
    /// NANOSECONDS; mixing the two is how this test once put a 1 000 ns cue under a 1 s offset.
    const SEC: i64 = 1_000_000_000;

    /// A positive offset draws a cue later, a negative one earlier, by exactly the offset.
    #[test]
    fn subtitle_offset_shifts_text_lookup_in_both_directions() {
        let _g = plx_base::testlock::serial();
        SHARED.sub_cues.lock().unwrap().clear();
        SHARED.playpos_ns.store(0, Relaxed);
        SHARED.desired_sub_idx.store(0, Relaxed);
        set_subtitle_offset(0);
        push_subtitle_text(0, SEC, 2 * SEC, "cue".into());

        assert_eq!(active_subtitle(SEC + SEC / 2).as_deref(), Some("cue"));

        set_subtitle_offset(1_000);
        assert_eq!(active_subtitle(SEC + SEC / 2), None, "a positive offset delays the caption");
        assert_eq!(active_subtitle(2 * SEC + SEC / 2).as_deref(), Some("cue"));
        assert_eq!(active_subtitle(3 * SEC), None, "…and ends it as late as it started it");

        // only a sidecar takes an advance (`subtitle_offset_range_ms`); the subtraction is the
        // one every store shares, so the embedded lookup still proves its direction
        sidecar::select_without_fetch_for_test(42);
        set_subtitle_offset(-1_000);
        assert_eq!(active_subtitle(SEC / 2).as_deref(), Some("cue"), "a negative one advances it");
        assert_eq!(active_subtitle(SEC + SEC / 2), None);

        set_subtitle_offset(0);
        sidecar::reset();
        SHARED.sub_cues.lock().unwrap().clear();
        SHARED.desired_sub_idx.store(-1, Relaxed);
    }

    /// **A delayed cue is still in the store when its moment comes.** Both stores prune cues more
    /// than two seconds behind the playhead on every push; under a +5 s offset the cue on screen
    /// at playhead 6.5 s is the one authored for 1.5 s, which the playhead-only floor had already
    /// thrown away when the next cue arrived.
    #[test]
    fn a_delayed_cue_survives_the_prune_until_the_offset_has_shown_it() {
        let _g = plx_base::testlock::serial();
        SHARED.sub_cues.lock().unwrap().clear();
        SHARED.sub_bitmaps.lock().unwrap().clear();
        SHARED.desired_sub_idx.store(0, Relaxed);
        set_subtitle_offset(5_000);
        SHARED.playpos_ns.store(0, Relaxed);
        push_subtitle_text(0, SEC, 2 * SEC, "early".into());
        push_subtitle_bitmap(0, SEC, 1920, 1080, vec![rect(0, 0, 8, 8)]);
        close_subtitle_bitmap(0, 2 * SEC);

        // the playhead reaches 6.5 s and the demuxer pushes the next cues, which prunes
        SHARED.playpos_ns.store(6 * SEC + SEC / 2, Relaxed);
        push_subtitle_text(0, 9 * SEC, 10 * SEC, "later".into());
        push_subtitle_bitmap(0, 9 * SEC, 1920, 1080, vec![rect(0, 0, 8, 8)]);

        let now = 6 * SEC + SEC / 2;
        assert_eq!(active_subtitle(now).as_deref(), Some("early"), "text");
        assert_eq!(active_bitmap_key(now), Some(SEC), "image");

        set_subtitle_offset(0);
        SHARED.playpos_ns.store(0, Relaxed);
        SHARED.sub_cues.lock().unwrap().clear();
        SHARED.sub_bitmaps.lock().unwrap().clear();
        SHARED.desired_sub_idx.store(-1, Relaxed);
    }

    /// **Raising the delay mid-playback finds the cues already read.** The demuxer never
    /// republishes a cue, so the stores must hold the whole window the LARGEST delay could ask
    /// for whatever the offset is now: a viewer at offset 0 who steps to +60 s wants the cue
    /// authored 60 s ago at once, not after the window has refilled. A floor that followed the
    /// current offset kept 2 s of history at 0 and blanked every raised delay.
    #[test]
    fn raising_the_delay_mid_playback_finds_the_cues_already_read() {
        let _g = plx_base::testlock::serial();
        SHARED.sub_cues.lock().unwrap().clear();
        SHARED.sub_bitmaps.lock().unwrap().clear();
        SHARED.desired_sub_idx.store(0, Relaxed);
        set_subtitle_offset(0);
        SHARED.playpos_ns.store(SEC, Relaxed);
        push_subtitle_text(0, SEC, 2 * SEC, "early".into());
        push_subtitle_bitmap(0, SEC, 1920, 1080, vec![rect(0, 0, 8, 8)]);
        close_subtitle_bitmap(0, 2 * SEC);

        // playback goes on at offset 0 and the demuxer pushes the next cues, which prunes
        SHARED.playpos_ns.store(61 * SEC + SEC / 2, Relaxed);
        push_subtitle_text(0, 62 * SEC, 63 * SEC, "later".into());
        push_subtitle_bitmap(0, 62 * SEC, 1920, 1080, vec![rect(0, 0, 8, 8)]);

        // the viewer steps straight to the latest delay: the clock is back at 1.5 s
        set_subtitle_offset(SUBTITLE_OFFSET_LATEST_MS);
        let now = 61 * SEC + SEC / 2;
        let text = active_subtitle(now);
        let image = active_bitmap_key(now);

        set_subtitle_offset(0);
        SHARED.playpos_ns.store(0, Relaxed);
        SHARED.sub_cues.lock().unwrap().clear();
        SHARED.sub_bitmaps.lock().unwrap().clear();
        SHARED.desired_sub_idx.store(-1, Relaxed);
        assert_eq!(text.as_deref(), Some("early"), "text: a raised delay finds the cue read at offset 0");
        assert_eq!(image, Some(SEC), "image: a raised delay finds the set read at offset 0");
    }

    /// **A new item starts with no timing offset.** The offset is a property of one subtitle track
    /// against one file; `reset_subtitle` is the new-item hook, and the last film's correction
    /// must not shift the next film's captions.
    #[test]
    fn a_new_item_starts_with_no_subtitle_offset() {
        let _g = plx_base::testlock::serial();
        set_subtitle_offset(2_000);
        assert_eq!(subtitle_offset_ms(), 2_000);
        reset_subtitle();
        assert_eq!(subtitle_offset_ms(), 0, "a new item must not inherit the offset");
        set_subtitle_offset(0);
    }

    /// **An advance is offered only where it can be served.** A sidecar holds its whole file, so
    /// it takes 60 s either way; an EMBEDDED track's cues arrive through the byte-bounded A/V
    /// queues (about 2 s ahead of the playhead at a high bitrate), so it takes a delay only. The
    /// clamp follows whichever kind is selected, including a negative offset left from a sidecar.
    #[test]
    fn an_embedded_track_takes_no_advance_and_a_sidecar_takes_sixty_seconds() {
        let _g = plx_base::testlock::serial();
        sidecar::reset();
        set_subtitle_offset(-1_000);
        assert_eq!(subtitle_offset_ms(), 0, "an embedded track (or Off) takes no advance");
        set_subtitle_offset(70_000);
        assert_eq!(subtitle_offset_ms(), 60_000, "an embedded track still clamps at the new ceiling");

        sidecar::select_without_fetch_for_test(42);
        set_subtitle_offset(-60_000);
        assert_eq!(subtitle_offset_ms(), -60_000, "a sidecar advances 60 s");
        set_subtitle_offset(i64::MIN);
        assert_eq!(subtitle_offset_ms(), -60_000);
        set_subtitle_offset(i64::MAX);
        assert_eq!(subtitle_offset_ms(), 60_000);

        // the kind decides, not the value's history: the same request on an embedded track is 0
        set_subtitle_offset(-2_000);
        sidecar::deselect();
        set_subtitle_offset(-2_000);
        assert_eq!(subtitle_offset_ms(), 0, "a negative offset never reaches an embedded track");
        set_subtitle_offset(0);
        sidecar::reset();
    }

    /// The offset can never wrap a timestamp: the lookups saturate at both ends of `i64`, and the
    /// setter clamps to the Timing rows' range.
    #[test]
    fn the_subtitle_clock_saturates_and_the_offset_clamps() {
        let _g = plx_base::testlock::serial();
        sidecar::select_without_fetch_for_test(42);
        set_subtitle_offset(60_000);
        assert_eq!(subtitle_clock_ns(i64::MIN), i64::MIN);
        set_subtitle_offset(-60_000);
        assert_eq!(subtitle_clock_ns(i64::MAX), i64::MAX);
        set_subtitle_offset(0);
        sidecar::reset();
    }
    /// **Under a delay, the eviction never takes a selected-track set the viewer has not seen
    /// while another track's set could go instead.** Every image track is decoded while subtitles
    /// are on (ff.rs), so the store holds the other tracks' sets beside the selected one's. With a
    /// +30 s offset the subtitle clock trails the playhead by 30 s, so the selected track's
    /// upcoming sets sit in the read-ahead — and the demuxer never republishes a set it has read.
    /// Evicting "the far end" dropped exactly those.
    #[test]
    fn a_delayed_selected_set_outlives_every_other_tracks_set() {
        let _g = plx_base::testlock::serial();
        SHARED.sub_bitmaps.lock().unwrap().clear();
        SHARED.desired_sub_idx.store(0, Relaxed);
        set_subtitle_offset(30_000);
        SHARED.playpos_ns.store(40 * SEC, Relaxed); // subtitle clock: 10 s
        // twelve display sets on each of two tracks, 2 s apart, from the subtitle clock on, the
        // demuxer's interleave (track 1 then track 0 at each moment). Sized against the budget so
        // the store overflows on a SELECTED-track push: one track-1 set is 3/5 of the budget, the
        // twelve selected sets together 4/5 of it.
        for i in 0..12 {
            let at = 10 * SEC + i * 2 * SEC;
            push_subtitle_bitmap(1, at, 1920, 1080, vec![rect_of(SUB_BITMAP_BUDGET * 3 / 5)]);
            push_subtitle_bitmap(0, at, 1920, 1080, vec![rect_of(SUB_BITMAP_BUDGET / 15)]);
        }
        let v = SHARED.sub_bitmaps.lock().unwrap();
        let total: usize = v.iter().map(|c| c.bytes()).sum();
        let selected: Vec<i64> = v.iter().filter(|c| c.track == 0).map(|c| c.start_ns / SEC).collect();
        drop(v);
        SHARED.sub_bitmaps.lock().unwrap().clear();
        SHARED.desired_sub_idx.store(-1, Relaxed);
        SHARED.playpos_ns.store(0, Relaxed);
        set_subtitle_offset(0);

        assert!(total <= SUB_BITMAP_BUDGET, "the store stayed inside its ceiling ({total} bytes)");
        assert_eq!(
            selected,
            (0..12).map(|i| 10 + 2 * i).collect::<Vec<_>>(),
            "every selected-track set still ahead of the subtitle clock must survive"
        );
    }

    /// **Held bytes are what is really allocated.** A rect whose index buffer has spare capacity
    /// (a `Vec` that grew by doubling) holds that capacity, so the budget counts it.
    #[test]
    fn a_rects_bytes_are_its_allocation_not_its_length() {
        let tight = rect(0, 0, 100, 10);
        let mut loose = rect(0, 0, 100, 10);
        loose.index.reserve_exact(1 << 20);
        assert_eq!(tight.bytes(), 1000 + 1024);
        assert!(loose.bytes() >= tight.bytes() + (1 << 20), "spare capacity is held, so it is counted");
    }

    /// **The bound is on what is held, and the cue on screen is never what pays for it.** A long run
    /// of large cues keeps the store inside [`SUB_BITMAP_BUDGET`] with the cue under the playhead
    /// present throughout; a single cue larger than the whole budget is held and shown (the budget
    /// bounds the OTHER cues). The renderer expands only that one visible set to RGBA, once per cue
    /// change, so the 4x never multiplies the store.
    #[test]
    fn a_run_of_large_cues_stays_bounded_and_the_visible_one_is_never_evicted() {
        let _g = plx_base::testlock::serial();
        SHARED.sub_bitmaps.lock().unwrap().clear();
        SHARED.desired_sub_idx.store(0, Relaxed);
        set_subtitle_offset(0);
        let mut worst = 0usize;
        for i in 0..40i64 {
            SHARED.playpos_ns.store(i * SEC + SEC / 2, Relaxed); // inside cue i, once it is pushed
            push_subtitle_bitmap(0, i * SEC, 1920, 1080, vec![rect_of(SUB_BITMAP_BUDGET / 5)]);
            let held: usize = SHARED.sub_bitmaps.lock().unwrap().iter().map(|c| c.bytes()).sum();
            worst = worst.max(held);
            assert_eq!(active_bitmap_key(i * SEC + SEC / 2), Some(i * SEC), "cue {i} is on screen");
        }
        assert!(worst <= SUB_BITMAP_BUDGET, "held {worst} bytes, budget {SUB_BITMAP_BUDGET}");

        // one cue larger than the whole budget, on an empty store
        SHARED.sub_bitmaps.lock().unwrap().clear();
        SHARED.playpos_ns.store(SEC / 2, Relaxed);
        push_subtitle_bitmap(0, 0, 1920, 1080, vec![rect_of(SUB_BITMAP_BUDGET * 2)]);
        let key = active_bitmap_key(SEC / 2).expect("an oversized cue is still on screen");
        let (_, _, rects) = bitmap_by_key(key).expect("and fetchable");
        assert_eq!(rects.len(), 1);
        // a later cue arriving while it is showing evicts the newcomer's slot, not the visible cue
        push_subtitle_bitmap(0, 10 * SEC, 1920, 1080, vec![rect_of(SUB_BITMAP_BUDGET / 5)]);
        assert_eq!(active_bitmap_key(SEC / 2), Some(0), "the visible oversized cue survives a push");

        SHARED.sub_bitmaps.lock().unwrap().clear();
        SHARED.desired_sub_idx.store(-1, Relaxed);
        SHARED.playpos_ns.store(0, Relaxed);
    }

    /// The renderer uploads what [`SubRect::to_rgba`] expands, so the expansion is the pixels:
    /// each index becomes its palette entry's straight-alpha RGBA, in row order.
    #[test]
    fn an_indexed_rect_expands_through_its_palette() {
        let mut palette = Box::new([[0u8; 4]; 256]);
        palette[1] = [10, 20, 30, 255];
        palette[255] = [1, 2, 3, 128];
        let r = SubRect { x: 0, y: 0, w: 3, h: 1, index: vec![0, 1, 255], palette };
        assert_eq!(r.to_rgba(), [0, 0, 0, 0, 10, 20, 30, 255, 1, 2, 3, 128]);
        assert_eq!(r.bytes(), 3 + 1024, "the store counts the indices and the palette");
    }

    /// **The supported window fits: 60 s of delay plus the demuxer's read-ahead, of a 4K-canvas
    /// PGS track.** The arithmetic is on `SUB_BITMAP_BUDGET`: 37 sets (a dense dialogue scene's
    /// one set per 2 s over 74 s) of a 2800x300 object, which is two lines of text on a
    /// 3840x2160 canvas.
    #[test]
    fn a_delayed_window_of_4k_canvas_sets_fits_the_budget() {
        let _g = plx_base::testlock::serial();
        SHARED.sub_bitmaps.lock().unwrap().clear();
        SHARED.desired_sub_idx.store(0, Relaxed);
        set_subtitle_offset(SUBTITLE_OFFSET_LATEST_MS);
        SHARED.playpos_ns.store(70 * SEC, Relaxed); // subtitle clock: 10 s
        for i in 0..37 {
            push_subtitle_bitmap(0, 10 * SEC + i * 2 * SEC, 3840, 2160, vec![rect(520, 1800, 2800, 300)]);
        }
        let kept = SHARED.sub_bitmaps.lock().unwrap().len();
        SHARED.sub_bitmaps.lock().unwrap().clear();
        SHARED.desired_sub_idx.store(-1, Relaxed);
        SHARED.playpos_ns.store(0, Relaxed);
        set_subtitle_offset(0);
        assert_eq!(kept, 37, "every set in the delayed window must be held");
    }

    /// The image-subtitle store, exercised as a display SET rather than a single bitmap. Three
    /// invariants moved when multi-rect landed and none of them is observable on the host except
    /// here: every rect of a set survives the round trip under ONE key (so a two-line PGS cue is
    /// not silently halved); a later set still closes the one still showing; and the RAM ceiling
    /// counts a set's rects together, so a multi-rect cue cannot smuggle bytes past the budget.
    ///
    /// Takes the crate-wide `testlock` — `SHARED` is a process-global the whole player shares.
    #[test]
    fn an_image_display_set_round_trips_whole_and_is_superseded_as_a_unit() {
        let _g = plx_base::testlock::serial();
        SHARED.sub_bitmaps.lock().unwrap().clear();
        SHARED.playpos_ns.store(0, Relaxed);
        SHARED.desired_sub_idx.store(0, Relaxed);

        // a two-rect set (dialogue plus a sign), authored on a DVD canvas
        push_subtitle_bitmap(
            0,
            1_000,
            720,
            480,
            vec![rect(60, 400, 600, 60), rect(100, 20, 200, 40)],
        );
        let key = active_bitmap_key(1_500).expect("the set should be active at its start");
        let (cw, ch, rects) = bitmap_by_key(key).expect("the active key must resolve");
        assert_eq!(
            (cw, ch),
            (720, 480),
            "the authoring canvas travels with the set"
        );
        assert_eq!(
            rects.len(),
            2,
            "BOTH rects must survive — rect 0 only was the bug"
        );
        assert_eq!((rects[1].x, rects[1].y), (100, 20));

        // the next set closes the open one AT ITS OWN START — a display set stays up until the
        // one that replaces it begins, so the handover is seamless and never double-shows
        push_subtitle_bitmap(0, 5_000, 720, 480, vec![rect(60, 400, 600, 60)]);
        assert_eq!(
            active_bitmap_key(4_999),
            Some(1_000),
            "the first set holds right up to the handover"
        );
        assert_eq!(
            active_bitmap_key(5_000),
            Some(5_000),
            "and the second takes over on that exact ns"
        );

        // an empty set is not a cue: it must not land and must not close what is showing
        push_subtitle_bitmap(0, 6_000, 720, 480, Vec::new());
        assert_eq!(active_bitmap_key(5_500), Some(5_000));

        // The byte budget charges a set for ALL its rects (two of a sixth of the budget each
        // here), and — the part that only matters once a set can be big — it must not evict the
        // cue the viewer is READING. The playhead sits inside the 5_000 cue, so that one has to
        // survive four sets of a third of the budget arriving from the demuxer's read-ahead; what
        // goes is the far end of that read-ahead.
        SHARED.playpos_ns.store(5_500, Relaxed);
        for i in 0..4 {
            push_subtitle_bitmap(
                0,
                10_000 + i,
                720,
                480,
                vec![rect_of(SUB_BITMAP_BUDGET / 6), rect_of(SUB_BITMAP_BUDGET / 6)],
            );
        }
        let v = SHARED.sub_bitmaps.lock().unwrap();
        let total: usize = v.iter().map(|c| c.bytes()).sum();
        assert!(
            total <= SUB_BITMAP_BUDGET,
            "the store stayed inside its ceiling ({total} bytes)"
        );
        assert!(
            v.iter().any(|c| c.start_ns == 5_000),
            "the cue under the playhead was not evicted"
        );
        drop(v);
        assert_eq!(
            active_bitmap_key(5_500),
            Some(5_000),
            "and it is still the one on screen"
        );

        // leave the globals as they were found — `desired_sub_idx` deliberately survives a reset
        // (shared.rs), so a test that leaves it selected changes what the NEXT one sees
        SHARED.sub_bitmaps.lock().unwrap().clear();
        SHARED.desired_sub_idx.store(-1, Relaxed);
    }
}

#[cfg(test)]
mod native_failure_regressions {
    use super::*;
    #[test]
    fn timeout_is_distinct_from_pipeline_refusal_and_resets_per_session() {
        let _serial = plx_base::testlock::serial();
        assert_eq!(runtime_failure(true, true, true, true), RuntimeFailure::LoadTimeout);
        assert_eq!(runtime_failure(false, false, true, false), RuntimeFailure::TvPipeline);
        SHARED.load_timed_out.store(true, Relaxed);
        SHARED.reset_session();
        assert!(!SHARED.load_timed_out.load(Relaxed));
    }
}

/// The two halves of "the playbar jumps on a seek", reported on an LG C3 (webOS 23), where
/// in-place seeking is disabled and every seek is a reload.
#[cfg(test)]
mod seek_hud_regressions {
    use super::*;

    /// **A reload keeps the file's duration; a real stop does not.** `teardown` zeroed it on
    /// every reload, so for the few hundred ms until the demuxer reopened the file the HUD drew
    /// the playhead at position ÷ 0 — the far left — while the clock beside it, which is the
    /// position alone, read correctly. Differential: before the fix the reload path called
    /// `reset_session` and the first assertion reads 0.
    #[test]
    fn a_reload_keeps_the_files_duration_and_a_stop_does_not() {
        let _serial = plx_base::testlock::serial();
        SHARED.reset_session();
        SHARED.duration_ns.store(5_400_000_000_000, Relaxed);
        SHARED.playpos_ns.store(1_200_000_000_000, Relaxed);
        SHARED.reset_session_for_reload();
        assert_eq!(duration_ns(), 5_400_000_000_000, "the same file is about to be reopened");
        assert_eq!(playpos_ns(), 0, "…and everything that IS the session's was still cleared");
        SHARED.reset_session();
        assert_eq!(duration_ns(), 0, "a real stop: the next item is a new file");
    }

    /// **A requested seek reads as Seeking at once**, not one pump pass later. `request_seek`
    /// sets the flag at the press; `pb_state` is republished only at the end of a pump pass, so
    /// a reader in between saw Playing with the scrub preview already cleared, and the HUD drew
    /// one frame of the pre-seek position before freezing on the target.
    #[test]
    fn a_requested_seek_reads_as_seeking_before_the_pump_republishes() {
        let _serial = plx_base::testlock::serial();
        let ps = crate::playback::PlaybackSession::IDLE;
        crate::playback::reset_player_control_for_test(&ps);
        SHARED.reset_session();
        TX.reset();
        SHARED.pb_state.store(PlaybackState::Playing as u8, Relaxed);
        assert_eq!(state(&ps), PlaybackState::Playing);
        // the REAL press path — and deliberately so: `tests/test_harness.py` holds that exactly
        // one place in the tree arms this flag, which a test arming it by hand would break
        request_seek(90_000_000_000);
        assert_eq!(state(&ps), PlaybackState::Seeking, "the pump has not run yet");
        assert!(state(&ps).is_busy(), "busy is what freezes the HUD's playhead on the target");
        assert_eq!(seek_display_ns(), 90_000_000_000, "…and this is the target it freezes on");
        SHARED.reset_session();
        TX.reset();
        crate::playback::reset_player_control_for_test(&ps);
    }
}

/// **Which rebuild is "Switching audio…"?** Only the one an audio-track pick asked for: it begins
/// at the pick, survives the reload, and ends at that session's first presented frame; a pick the
/// server never rebuilt for expires; anything else (a seek's reload, no pick at all) is Buffering.
#[cfg(test)]
mod audio_switch_tests {
    use super::*;
    use std::sync::atomic::Ordering::Relaxed;

    fn reset() {
        SHARED.reset_session();
        SHARED.audio_switch.store(shared::AudioSwitch::None as u8, Relaxed);
    }

    #[test]
    fn the_caption_is_chosen_by_the_pick_and_only_for_buffering() {
        use PlaybackState as S;
        assert_eq!(busy_caption_with(S::Buffering, true), plx_platform::i18n::msg::widgets_status_switching_audio_c());
        assert_eq!(busy_caption_with(S::Buffering, false), S::Buffering.caption());
        // a switch never renames the other waits
        assert_eq!(busy_caption_with(S::Seeking, true), S::Seeking.caption());
        assert_eq!(busy_caption_with(S::Connecting, true), S::Connecting.caption());
        assert_eq!(busy_caption_with(S::Resolving, true), S::Resolving.caption());
    }

    #[test]
    fn a_pick_names_the_rebuild_through_the_reload_until_the_first_frame() {
        let _g = plx_base::testlock::serial();
        reset();
        assert!(!audio_switching(), "no pick, no switch: a seek's reload is Buffering");
        note_audio_pick();
        assert!(audio_switching(), "the pick's rebuild is flying");
        SHARED.reset_session_for_reload();
        assert!(audio_switching(), "the reload keeps the pick's meaning");
        let epoch = SHARED.begin_native_session().expect("session");
        sf_on_event(epoch, 0, 7_000_000_000, std::ptr::null());
        assert!(!audio_switching(), "the first picture ends the switch");
        // a later seek-style reload is no longer a switch
        SHARED.reset_session_for_reload();
        assert!(!audio_switching());
        SHARED.retire_native_session(epoch);
        reset();
    }

    #[test]
    fn a_pick_whose_rebuild_never_came_expires_and_a_new_item_forgets_it() {
        let _g = plx_base::testlock::serial();
        reset();
        note_audio_pick();
        SHARED.audio_pick_ms.store(i64::from(vclock_ms()) - shared::AUDIO_PICK_WINDOW_MS - 1, Relaxed);
        assert!(!audio_switching(), "a refused rebuild must not name a later one");
        note_audio_pick();
        assert!(audio_switching());
        SHARED.reset_session();
        assert!(!audio_switching(), "a full reset (new item, teardown) ends it");
        reset();
    }
}
