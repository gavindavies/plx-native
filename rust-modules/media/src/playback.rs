//! **The playback session**: what the player is playing now, and who owns each native Load.
//!
//! [`PlaybackSession`] is the main thread's description of the playback: the file's URL, the
//! declaration the Load is made with (codecs, frame rate, source raster), the audio and subtitle
//! tracks picked, and the HUD's title lines. `localplay::install` writes it before a start; the
//! engine reads it to compose the Load and the track menus read it to draw their checkmarks.
//!
//! Beside it sits the one piece of state a worker thread touches: the **Load reducer**
//! ([`PLAYER_CONTROL`]). A Load is asynchronous (`sf_load` returns on the media thread), so every
//! start runs as a transaction: [`begin_route_start`] reserves it, [`prepare_route_start`] commits
//! to tearing the old Engine down, [`claim_route_start_attempt`] mints the exact physical attempt,
//! and only the media thread's result for THAT attempt ([`publish_route_start_result`], drained on
//! the main thread by [`drain_route_start_results`]) may settle it. A late result from a replaced
//! attempt matches nothing and is dropped, so a slow Load can never mark its successor started or
//! failed.

use std::os::raw::c_char;

/// The audio track a playback carries, frozen at the moment it was picked.
#[derive(Clone, PartialEq, Debug)]
pub struct CarriedAudio {
    /// The track's id in the playing item (`metadata::Stream::id`).
    pub sid: i64,
    /// The demuxer-facing ordinal (`metadata::audio_ordinal`): what `set_audio_track` feeds.
    pub ordinal: i32,
    pub codec: String,
    pub channels: i64,
}

impl CarriedAudio {
    /// The track `s`, at the container ordinal `ordinal`.
    pub fn from_stream(s: &plx_data::metadata::Stream, ordinal: i32) -> Self {
        CarriedAudio { sid: s.id, ordinal, codec: s.codec.to_lowercase(), channels: s.channels }
    }
}

/// What the player is playing now. Main-thread only; a screen is shown a copy
/// ([`PlaybackSession::publication`]).
#[derive(Clone)]
pub struct PlaybackSession {
    /// The last fresh start was refused because a timed-out Load is still inside `sf_load` (an
    /// `AbandonedLoad` owns the one native object). No Engine exists, so nothing else would
    /// publish the failure; `player::state` derives `Error` (cause `load_timeout`) from it.
    /// Cleared by the next start attempt.
    pub native_start_refused: bool,
    /// A resume target not yet proven by a presented frame, so a Retry after a failed start
    /// resumes where the viewer asked for rather than at zero. Cleared by
    /// [`confirm_resume_presented`].
    requested_resume_ns: i64,
    /// The stream URL (`file://…`; empty = nothing to play).
    url: String,
    /// The audio track playing, `None` before one is known.
    cur_audio: Option<CarriedAudio>,
    /// The subtitle picked (`metadata::Stream::id`, 0 = none).
    cur_sub_sid: i64,
    /// The language the Subtitles menu groups as the viewer's own (`metadata::sub_layout`).
    cur_sub_pref_lang: Option<String>,
    /// The Load declaration: the video and audio codecs FFmpeg names them by, and the frame rate
    /// (0 = unknown; omitted from the Load's esInfo).
    stream_vcodec: String,
    stream_acodec: String,
    stream_fps: f64,
    /// The source's frame size (0 = unknown), which bounds the sink envelope the Load declares.
    source_raster: (u16, u16),
    /// HUD strings as fixed NUL-terminated C buffers, so `title_cptr`/`ctxline_cptr` hand
    /// `draw_text` a pointer that stays valid for the whole frame.
    title: [c_char; 128],
    ctxline: [c_char; 96],
    /// This frame's millisecond stamp, mirrored from `player::machine::Player::set_now`.
    now_ms: u32,
}

impl PlaybackSession {
    /// Nothing playing.
    pub const IDLE: PlaybackSession = PlaybackSession {
        native_start_refused: false,
        requested_resume_ns: 0,
        url: String::new(),
        cur_audio: None,
        cur_sub_sid: 0,
        cur_sub_pref_lang: None,
        stream_vcodec: String::new(),
        stream_acodec: String::new(),
        stream_fps: 0.0,
        source_raster: (0, 0),
        title: [0; 128],
        ctxline: [0; 96],
        now_ms: 0,
    };

    /// The per-frame copy a screen is shown: the loop publishes it into the rig once per frame,
    /// so a screen never holds the `&mut` that would let it write the session from a draw.
    pub fn publication(&self) -> PlaybackSession {
        self.clone()
    }

    /// The frame tick, written once per iteration by `player::machine::Player::set_now`.
    pub fn set_now(&mut self, now_ms: u32) {
        self.now_ms = now_ms;
    }
}

impl Default for PlaybackSession {
    fn default() -> Self {
        Self::IDLE
    }
}

/// A borrowable idle session, for a test that builds a `Cx` and has no playback in it.
#[cfg(any(test, feature = "test-support"))]
pub fn idle_session_for_test() -> &'static PlaybackSession {
    static IDLE: std::sync::OnceLock<PlaybackSession> = std::sync::OnceLock::new();
    IDLE.get_or_init(|| PlaybackSession::IDLE)
}

// ---- the stream -------------------------------------------------------------------------------

pub fn url(ps: &PlaybackSession) -> String {
    ps.url.clone()
}
/// Is there a stream URL at all? The in-place twin of [`url`], for a caller that only wants the
/// emptiness.
pub fn has_url(ps: &PlaybackSession) -> bool {
    !ps.url.is_empty()
}
pub fn set_url(ps: &mut PlaybackSession, s: &str) {
    ps.url = s.to_owned()
}
pub fn clear_url(ps: &mut PlaybackSession) {
    ps.url.clear()
}

/// The whole Load declaration, in ONE write: these fields describe one stream, and a half-applied
/// set is a payload that describes nothing real.
pub fn set_stream_declaration(ps: &mut PlaybackSession, vc: &str, ac: &str, fps: f64) {
    ps.stream_vcodec = vc.to_owned();
    ps.stream_acodec = ac.to_owned();
    ps.stream_fps = fps;
}
pub fn stream_vcodec(ps: &PlaybackSession) -> String {
    ps.stream_vcodec.clone()
}
pub fn stream_acodec(ps: &PlaybackSession) -> String {
    ps.stream_acodec.clone()
}
/// The audio switch's half of the declaration: a native switch feeds another track of the same
/// file, whose codec the next Load must name.
pub fn set_stream_acodec(ps: &mut PlaybackSession, codec: &str) {
    ps.stream_acodec = codec.to_owned();
}
pub fn stream_fps(ps: &PlaybackSession) -> f64 {
    ps.stream_fps
}
pub fn set_stream_source_raster(ps: &mut PlaybackSession, w: u16, h: u16) {
    ps.source_raster = (w, h);
}
/// The largest raster the Load may promise the sink: the source's own (0 on an axis nobody
/// measured).
pub fn sink_max_raster(ps: &PlaybackSession) -> (u16, u16) {
    ps.source_raster
}

// ---- the tracks -------------------------------------------------------------------------------

/// The track a playback starts on, recorded without a switch (the demuxer already feeds it).
pub fn set_cur_audio(ps: &mut PlaybackSession, audio: CarriedAudio) {
    ps.cur_audio = Some(audio);
}
pub fn cur_audio(ps: &PlaybackSession) -> Option<&CarriedAudio> {
    ps.cur_audio.as_ref()
}
/// The playing audio track's id, 0 before one is known.
pub fn cur_audio_sid(ps: &PlaybackSession) -> i64 {
    ps.cur_audio.as_ref().map_or(0, |a| a.sid)
}
pub fn cur_sub_sid(ps: &PlaybackSession) -> i64 {
    ps.cur_sub_sid
}
pub fn cur_sub_pref_lang(ps: &PlaybackSession) -> Option<&str> {
    ps.cur_sub_pref_lang.as_deref()
}
pub fn set_sub_pref_lang(ps: &mut PlaybackSession, lang: &str) {
    ps.cur_sub_pref_lang = (!lang.is_empty()).then(|| lang.to_owned());
}

/// Commit an audio-track pick: feed the chosen track of the same file (`audio.ordinal` is its
/// container ordinal, `metadata::audio_ordinal`).
pub fn commit_audio_selection(ps: &mut PlaybackSession, audio: CarriedAudio) {
    let (ordinal, codec) = (audio.ordinal, audio.codec.clone());
    ps.cur_audio = Some(audio);
    crate::player::request_audio_track(ps, ordinal, &codec);
    // the reload now loading is this pick's: the read-out says "Switching audio…"
    crate::player::note_audio_pick();
}

/// Commit a subtitle pick (`sub_idx` -1 = Off or a sidecar, else the embedded track's ordinal
/// among the file's subtitle tracks). A timing offset was tuned against the track that was
/// showing, so a DIFFERENT pick starts at zero; re-committing the same track keeps it.
pub fn commit_subtitle_selection(ps: &mut PlaybackSession, sub_idx: i32, stream_id: i64) {
    if stream_id != ps.cur_sub_sid {
        crate::player::set_subtitle_offset(0);
    }
    crate::player::request_subtitle(sub_idx);
    ps.cur_sub_sid = stream_id;
}

// ---- the HUD's title --------------------------------------------------------------------------

/// Copy `src` into a fixed NUL-terminated buffer, truncated at a character boundary.
fn copy_cstr(dst: &mut [c_char], src: &str) {
    let max = dst.len() - 1;
    let mut n = src.len().min(max);
    while !src.is_char_boundary(n) {
        n -= 1;
    }
    for (d, &b) in dst.iter_mut().zip(&src.as_bytes()[..n]) {
        *d = b as c_char;
    }
    dst[n] = 0;
}

pub fn set_hud_title(ps: &mut PlaybackSession, title: &str, ctxline: &str) {
    copy_cstr(&mut ps.title, title);
    copy_cstr(&mut ps.ctxline, ctxline);
}
pub fn title_cptr(ps: &PlaybackSession) -> *const c_char {
    ps.title.as_ptr()
}
pub fn ctxline_cptr(ps: &PlaybackSession) -> *const c_char {
    ps.ctxline.as_ptr()
}

// ---- resume -----------------------------------------------------------------------------------

/// Record the resume target a start was asked for, until a frame proves it.
pub fn note_requested_resume(ps: &mut PlaybackSession, resume_ns: i64) {
    ps.requested_resume_ns = resume_ns.max(0);
}
/// The resume target no frame has proven yet (0 when there is none).
pub fn unpresented_resume_ns(ps: &PlaybackSession) -> i64 {
    ps.requested_resume_ns
}
/// A frame of this playback was presented: the resume it was started with is spent.
pub fn confirm_resume_presented(ps: &mut PlaybackSession) {
    ps.requested_resume_ns = 0;
}

// ---- the Load reducer -------------------------------------------------------------------------

/// Identity of one prepared start. A transaction may mint several never-reused
/// [`RouteStartAttempt`]s; the attempt, not the transaction, keeps a late Load result from settling
/// a retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteStartTransaction {
    serial: u64,
}

/// One exact physical Load attempt within a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteStartAttempt {
    serial: u64,
    attempt: u64,
}

impl RouteStartAttempt {
    /// An attempt no reducer minted, for a test that needs an Engine to carry one.
    #[cfg(any(test, feature = "test-support"))]
    pub fn fixture(serial: u64, attempt: u64) -> Self {
        RouteStartAttempt { serial, attempt }
    }
}

/// What the native half of a start came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteStartResult {
    Started,
    /// There was nothing to play (no URL).
    NoRoute,
    StartFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlPhase {
    /// Nothing playing, or a completed stop.
    Idle,
    /// A live Engine whose start has settled.
    Stable,
    /// A start transaction owns the next Load, which has not been attempted yet.
    Prepared(u64),
    /// One exact physical Load attempt is in flight: `(transaction, attempt)`.
    Starting(u64, u64),
    /// The transaction's Load failed; no Engine plays.
    Failed(u64),
    /// The synchronous teardown of a stop owns the loop.
    Stopping,
}

struct PlayerControl {
    phase: ControlPhase,
    next_action: u64,
    next_start_attempt: u64,
    /// Load results crossed back from the media thread, drained on the main thread only after the
    /// Engine is installed, so a fast `sf_load` cannot publish Stable in front of its own slot.
    start_results: Vec<(RouteStartAttempt, RouteStartResult)>,
    last_start_result: Option<(RouteStartAttempt, RouteStartResult)>,
}

static PLAYER_CONTROL: std::sync::Mutex<PlayerControl> = std::sync::Mutex::new(PlayerControl {
    phase: ControlPhase::Idle,
    next_action: 0,
    next_start_attempt: 0,
    start_results: Vec::new(),
    last_start_result: None,
});

fn control() -> std::sync::MutexGuard<'static, PlayerControl> {
    PLAYER_CONTROL.lock().unwrap_or_else(|e| e.into_inner())
}

/// The transaction a start is pending under, if one is.
pub fn pending_route_start() -> Option<RouteStartTransaction> {
    match control().phase {
        ControlPhase::Prepared(serial) | ControlPhase::Starting(serial, _) => Some(RouteStartTransaction { serial }),
        _ => None,
    }
}

/// Return or reserve the transaction for the next Engine. A start already pending keeps its own;
/// from any settled phase a new one is minted. `None` only while a stop's teardown owns the loop.
pub fn begin_route_start() -> Option<RouteStartTransaction> {
    let mut control = control();
    match control.phase {
        ControlPhase::Prepared(serial) | ControlPhase::Starting(serial, _) => Some(RouteStartTransaction { serial }),
        ControlPhase::Idle | ControlPhase::Stable | ControlPhase::Failed(_) => {
            control.next_action += 1;
            let serial = control.next_action;
            control.phase = ControlPhase::Prepared(serial);
            Some(RouteStartTransaction { serial })
        }
        ControlPhase::Stopping => None,
    }
}

/// The caller is about to replace the native Engine for `ticket`: does the transaction still own
/// the start?
pub fn prepare_route_start(ticket: RouteStartTransaction) -> bool {
    matches!(control().phase,
        ControlPhase::Prepared(serial) | ControlPhase::Starting(serial, _) if serial == ticket.serial)
}

/// Mint the physical Load attempt, once teardown of the old Engine has completed. A second attempt
/// always receives a different id, even inside the same transaction.
pub fn claim_route_start_attempt(ticket: RouteStartTransaction) -> Option<RouteStartAttempt> {
    let mut control = control();
    if control.phase != ControlPhase::Prepared(ticket.serial) {
        return None;
    }
    control.next_start_attempt += 1;
    let attempt = control.next_start_attempt;
    control.phase = ControlPhase::Starting(ticket.serial, attempt);
    Some(RouteStartAttempt { serial: ticket.serial, attempt })
}

/// The preparation of a pending start failed before any Load: the transaction ends `Failed`.
pub fn reject_route_start_preparation(ticket: RouteStartTransaction) -> bool {
    let mut control = control();
    match control.phase {
        ControlPhase::Prepared(serial) if serial == ticket.serial => {
            control.phase = ControlPhase::Failed(serial);
            true
        }
        _ => false,
    }
}

/// Settle a transaction that never reached a callable native thread (no callback can race it).
pub fn abort_route_start(ticket: RouteStartTransaction, _result: RouteStartResult) -> bool {
    let mut control = control();
    match control.phase {
        ControlPhase::Prepared(serial) | ControlPhase::Starting(serial, _) if serial == ticket.serial => {
            control.phase = ControlPhase::Failed(serial);
            true
        }
        _ => false,
    }
}

/// Publish the native half of one start. Only the attempt in flight may settle it.
pub fn settle_route_start(_ps: &mut PlaybackSession, ticket: RouteStartAttempt, result: RouteStartResult) -> bool {
    let mut control = control();
    if control.phase != ControlPhase::Starting(ticket.serial, ticket.attempt) {
        return false;
    }
    control.last_start_result = Some((ticket, result));
    control.phase = match result {
        RouteStartResult::Started => ControlPhase::Stable,
        RouteStartResult::NoRoute | RouteStartResult::StartFailed => ControlPhase::Failed(ticket.serial),
    };
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteStartStatus {
    Pending,
    Started,
    Failed,
    /// A later physical Load replaced the observed attempt before its observer consumed the
    /// completion. Following this token keeps the observer attached to the Engine that now owns
    /// the screen instead of tearing it down as if the old result were a failure.
    Superseded(RouteStartAttempt),
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveEngineStartRelation {
    /// The caller rediscovered the Engine which already owns this exact in-flight Load.
    CurrentAttempt,
    /// No start is waiting; the live Engine is an ordinary idempotent start.
    NoPendingRoute,
    /// A different pending start requires a new Engine and cannot borrow this one.
    Conflict(RouteStartTransaction),
}

/// Classify a start request which found a live Engine, comparing the transaction and the attempt
/// under one lock.
pub fn classify_live_engine_start(existing: RouteStartAttempt) -> LiveEngineStartRelation {
    match control().phase {
        ControlPhase::Starting(serial, attempt) if serial == existing.serial && attempt == existing.attempt => {
            LiveEngineStartRelation::CurrentAttempt
        }
        ControlPhase::Prepared(serial) | ControlPhase::Starting(serial, _) => {
            LiveEngineStartRelation::Conflict(RouteStartTransaction { serial })
        }
        _ => LiveEngineStartRelation::NoPendingRoute,
    }
}

/// Observe one exact physical start without consuming another subsystem's result.
pub fn route_start_status(ticket: RouteStartAttempt) -> RouteStartStatus {
    let control = control();
    if let ControlPhase::Starting(serial, attempt) = control.phase {
        let current = RouteStartAttempt { serial, attempt };
        return if current == ticket {
            RouteStartStatus::Pending
        } else if current.attempt > ticket.attempt {
            RouteStartStatus::Superseded(current)
        } else {
            RouteStartStatus::Stale
        };
    }
    // A terminal result is observable only while the phase still says that Load owns the
    // playback; after a teardown it is history, and following it would resurrect a dead Engine.
    let completed = match (control.phase, control.last_start_result) {
        (ControlPhase::Stable, Some((attempt, RouteStartResult::Started))) => Some((attempt, RouteStartStatus::Started)),
        (ControlPhase::Failed(serial), Some((attempt, RouteStartResult::NoRoute | RouteStartResult::StartFailed)))
            if attempt.serial == serial =>
        {
            Some((attempt, RouteStartStatus::Failed))
        }
        _ => None,
    };
    match completed {
        Some((attempt, status)) if attempt == ticket => status,
        Some((attempt, _)) if attempt.attempt > ticket.attempt => RouteStartStatus::Superseded(attempt),
        _ => RouteStartStatus::Stale,
    }
}

/// Media-thread half of the start handshake. Queued rather than settled here: `sf_load` can
/// return before the spawning main thread has installed its Engine.
pub fn publish_route_start_result(ticket: RouteStartAttempt, result: RouteStartResult) {
    control().start_results.push((ticket, result));
}

/// Main-thread publication point for every completed Load call. Late results are drained too;
/// [`settle_route_start`] rejects their attempt.
pub fn drain_route_start_results(ps: &mut PlaybackSession) {
    let results = std::mem::take(&mut control().start_results);
    for (ticket, result) in results {
        let _ = settle_route_start(ps, ticket, result);
    }
}

/// A playback which had started later proved terminal (a demux or native failure).
pub fn fail_current_engine() {
    let mut control = control();
    let serial = match control.phase {
        ControlPhase::Prepared(serial) | ControlPhase::Starting(serial, _) => serial,
        ControlPhase::Stable => {
            control.next_action += 1;
            control.next_action
        }
        ControlPhase::Failed(_) | ControlPhase::Idle | ControlPhase::Stopping => return,
    };
    control.phase = ControlPhase::Failed(serial);
}

/// The Engine is being torn down. A stop (`for_reload == false`) fences the loop until
/// [`finish_engine_teardown`]; a reload hands an in-flight attempt's transaction back to
/// `Prepared`, so the replacement mints a fresh attempt the old Load's result cannot match.
pub fn begin_engine_teardown(for_reload: bool) {
    let mut control = control();
    if !for_reload {
        control.phase = ControlPhase::Stopping;
    } else if let ControlPhase::Starting(serial, _) = control.phase {
        control.phase = ControlPhase::Prepared(serial);
    }
}

/// The synchronous teardown a stop announced has returned: `Idle`. Only `Stopping` moves; a phase
/// a newer start has already replaced is left as it is.
pub fn finish_engine_teardown() {
    let mut control = control();
    if control.phase == ControlPhase::Stopping {
        control.phase = ControlPhase::Idle;
    }
}

/// The reducer's phase, for a log line: "refused" alone cannot be diagnosed from a log.
pub fn control_phase_label() -> String {
    format!("{:?}", control().phase)
}

/// Put the reducer back to a settled, live playback, as a test that installs an Engine by hand
/// needs it.
#[cfg(any(test, feature = "test-support"))]
pub fn reset_player_control_for_test(_ps: &PlaybackSession) {
    let mut control = control();
    control.phase = ControlPhase::Stable;
    control.next_action = 0;
    control.start_results.clear();
    control.last_start_result = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_attempt_in_flight_settles_its_start() {
        let _serial = plx_base::testlock::serial();
        let mut ps = PlaybackSession::IDLE;
        reset_player_control_for_test(&ps);
        let t = begin_route_start().unwrap();
        assert!(prepare_route_start(t));
        let first = claim_route_start_attempt(t).unwrap();
        begin_engine_teardown(true);
        let second = claim_route_start_attempt(t).unwrap();
        assert_ne!(first, second, "a retry is a new attempt");
        publish_route_start_result(first, RouteStartResult::StartFailed);
        drain_route_start_results(&mut ps);
        assert_eq!(route_start_status(second), RouteStartStatus::Pending, "the old Load's failure matched nothing");
        assert_eq!(route_start_status(first), RouteStartStatus::Superseded(second));
        publish_route_start_result(second, RouteStartResult::Started);
        drain_route_start_results(&mut ps);
        assert_eq!(route_start_status(second), RouteStartStatus::Started);
    }

    #[test]
    fn a_stop_fences_new_starts_until_its_teardown_returns() {
        let _serial = plx_base::testlock::serial();
        reset_player_control_for_test(&PlaybackSession::IDLE);
        begin_engine_teardown(false);
        assert_eq!(begin_route_start(), None);
        finish_engine_teardown();
        assert!(begin_route_start().is_some());
    }

    #[test]
    fn the_hud_title_is_truncated_at_a_character_boundary() {
        let mut ps = PlaybackSession::IDLE;
        let long = "é".repeat(100);
        set_hud_title(&mut ps, &long, "Films");
        let title = unsafe { std::ffi::CStr::from_ptr(title_cptr(&ps)) }.to_str().unwrap();
        assert_eq!(title.chars().count(), 63, "127 bytes hold 63 two-byte characters");
        assert_eq!(unsafe { std::ffi::CStr::from_ptr(ctxline_cptr(&ps)) }.to_str().unwrap(), "Films");
    }
}
