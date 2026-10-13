//! The app's side of the foreground/background lifecycle: the loop's adapter for the player's
//! resume machine ([`PlayerForegroundActuator`]) and the names the loop spells for it.
//!
//! The machine itself (`ForegroundLifecycle`, already a tested `State + Event -> Effects`
//! machine), the foreground driver and the transport-pause contract (`paused`,
//! `viewer_paused`, `set_transport_paused`, ...) describe the PLAYER's transport, so they live in
//! [`plx_media::player::lifecycle`] (module-layer step L12: `player` may not name `app`) and are
//! re-exported here, which keeps every `app` caller spelling the names it always did. Moved out of
//! `app.rs` verbatim in phase 1a (a pure move; `pub(crate)` widening only).

use super::*;

pub(crate) use plx_media::player::lifecycle::{
    clock_for_suspend_now, drive_foreground, park_pending_foreground_flight, paused,
    poll_foreground_flight, poll_foreground_load, resume_if_paused, set_paused,
    set_transport_paused, transport_target, viewer_paused, FlightLanding, ForegroundActivation,
    ForegroundActuator, ForegroundClock, ForegroundInput, ForegroundLifecycle, ForegroundLoadStart,
    ForegroundLoadStatus,
};
// Only `app::run`'s hostsim lifecycle tests name the machine's state, so a re-export outside
// them would be an unused import.

pub(crate) struct PlayerForegroundActuator<'a> {
    /// The native session slot — phase 9's replacement for the `&MainThread` this held, and the
    /// same value every other player call in the loop is threaded (`player::adapter`).
    pub(crate) pa: &'a mut plx_media::player::adapter::PlayerAdapter,
    pub(crate) repause_at: &'a mut i64,
}

/// What the foreground's `Load` does with the Original rollback's answer: the rebase is a flight
/// (`Pending`), or — once it landed and reloaded — the exact attempt to follow.
fn foreground_load_start(
    recovery: plx_media::player::ForegroundOriginalRecovery,
) -> ForegroundLoadStart<plx_media::playback::RouteStartAttempt> {
    use plx_media::player::ForegroundOriginalRecovery as Recovery;
    match recovery {
        Recovery::NotOriginal | Recovery::RetryPrepared => ForegroundLoadStart::Failed,
        Recovery::Pending => ForegroundLoadStart::Pending,
        Recovery::Tracking(attempt) => ForegroundLoadStart::Launched(attempt),
        Recovery::Terminal => ForegroundLoadStart::Terminal,
    }
}

impl ForegroundActuator for PlayerForegroundActuator<'_> {
    type Attempt = plx_media::playback::RouteStartAttempt;

    fn prepare_resume(&mut self, ps: &mut plx_media::playback::PlaybackSession, resume_ns: i64) -> plx_media::player::ResumeStart {
        plx_media::player::begin_resume(ps, resume_ns)
    }

    fn land_prepare(&mut self, ps: &mut plx_media::playback::PlaybackSession) -> FlightLanding<plx_media::player::ResumeOutcome> {
        match plx_media::player::land_resume(ps) {
            Some(outcome) => FlightLanding::Landed(outcome),
            // `land_resume` reaped a stale landing first: nothing outstanding means the flight
            // ended (a Back, a newer request) and its ender owns what comes next.
            None if plx_media::playback::resume_flight_outstanding() => FlightLanding::Waiting,
            None => FlightLanding::Stale,
        }
    }

    fn land_recovery(&mut self, ps: &mut plx_media::playback::PlaybackSession) -> FlightLanding<ForegroundLoadStart<Self::Attempt>> {
        use plx_media::player::RollbackLanding;
        match plx_media::player::land_engineless_rollback(ps, self.pa) {
            RollbackLanding::Waiting => FlightLanding::Waiting,
            RollbackLanding::Stale => FlightLanding::Stale,
            RollbackLanding::Landed(recovery) => FlightLanding::Landed(foreground_load_start(recovery)),
        }
    }

    fn before_load(&mut self, resume_ns: i64, clock: ForegroundClock) {
        if matches!(clock, ForegroundClock::Paused) {
            *self.repause_at = resume_ns;
            set_resume_pend(true);
            plx_media::player::TX.begin_paused_seek();
        }
    }

    fn start_load(&mut self, ps: &mut plx_media::playback::PlaybackSession) -> ForegroundLoadStart<Self::Attempt> {
        let first = plx_media::player::start_bufferfeed_tracked(ps, self.pa);
        if matches!(first, plx_media::player::BufferfeedStartOutcome::Failed) {
            // A synchronous failure inside an Original trial has no Engine for player::pump to
            // recover. Take the same explicit rollback edge here and follow its exact HLS Load.
            return foreground_load_start(plx_media::player::recover_failed_foreground_original(ps));
        }
        match first {
            plx_media::player::BufferfeedStartOutcome::AlreadyRunning => {
                ForegroundLoadStart::AlreadyRunning
            }
            plx_media::player::BufferfeedStartOutcome::Launched(attempt) => {
                ForegroundLoadStart::Launched(attempt)
            }
            plx_media::player::BufferfeedStartOutcome::Failed => unreachable!("handled above"),
        }
    }

    fn load_status(&mut self, attempt: Self::Attempt) -> ForegroundLoadStatus<Self::Attempt> {
        match plx_media::playback::route_start_status(attempt) {
            plx_media::playback::RouteStartStatus::Pending => ForegroundLoadStatus::Pending,
            plx_media::playback::RouteStartStatus::Started => ForegroundLoadStatus::Started,
            plx_media::playback::RouteStartStatus::Failed => ForegroundLoadStatus::Failed,
            plx_media::playback::RouteStartStatus::Superseded(replacement) => {
                ForegroundLoadStatus::Superseded(replacement)
            }
            plx_media::playback::RouteStartStatus::Stale => ForegroundLoadStatus::Stale,
        }
    }

    fn after_load(
        &mut self,
        ps: &mut plx_media::playback::PlaybackSession,
        attempt: Option<Self::Attempt>,
        clock: ForegroundClock,
        started: bool,
    ) {
        if matches!(clock, ForegroundClock::Paused) {
            if started {
                set_resume_pend(true);
            } else {
                plx_media::player::TX.finish_seek_preroll();
            }
        }
        if !started && attempt.is_some() {
            // `sf_load == 0` publishes Error but intentionally leaves the Engine available for
            // diagnostics. Retire it only if it still owns the observed token: player::pump may
            // already have launched a healthy replacement before foreground polls this result.
            let _ = plx_media::player::suspend_bufferfeed_if_attempt(ps, self.pa, attempt.unwrap());
        }
    }

    fn play_clock(&mut self) -> bool {
        // start_bufferfeed has installed the Initial native-clock hold from TX.paused. Publish
        // Play through the synchronized reducer so that hold and the feed gate reopen together.
        !paused() || set_transport_paused(self.pa, false)
    }
}
