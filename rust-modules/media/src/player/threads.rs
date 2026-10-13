//! player::threads — the media thread beside the demuxer (`load_thread`) + the SendPtr spawn
//! seam. The demux thread body is `ff::demux` (libavformat over a custom AVIO on the file).
use super::SHARED;
use std::os::raw::c_char;
use std::sync::atomic::Ordering;

/// raw ptr we assert is Send for the spawn (the boxes/queue outlive the thread).
pub struct SendPtr<T>(pub *mut T);
unsafe impl<T> Send for SendPtr<T> {}

/// media/load thread: construct + Load (uid=NULL). The library owns its own GMainContext + loop,
/// and callbacks arrive on its thread — but Load itself is SYNCHRONOUS here and on some chassis
/// blocks for a long time inside video-output init (issue #74, k5lp/k3lp), which is why the C seam
/// refuses every other verb until it returns (`g_load_returned`) and the pump bounds the wait
/// with `NATIVE_LOAD_BUDGET`.
pub fn load_thread(
    payload: SendPtr<c_char>,
    native_epoch: u32,
    route_start: Option<crate::playback::RouteStartAttempt>,
) {
    super::log("SMP: calling Load (uid=NULL)");
    let ok = unsafe { super::sink().load(payload.0, native_epoch) };
    super::log(&format!("SMP: Load returned ok={ok}"));
    // `/tmp/plxnative-holdload[=ms]`: on demand, hold the flip below so issue #74 D.1's budget
    // (the pump's "deferring" line, then, past `NATIVE_LOAD_BUDGET`, the failure read-out) is
    // observable on a real television without a set that hangs here for real. Read AFTER
    // `sf_load` returns and BEFORE `mark_native_load_returned`, matching where this needs to bite.
    if let Some(ms) = plx_base::devtrig::holdload_delay_ms() {
        super::log(&format!(
            "holdload: armed — holding the Load-returned flag for {ms}ms"
        ));
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
    // issue #74 D.1: flip the epoch-scoped gate BEFORE the route ticket is published and BEFORE
    // `load_failed` is set below — `pump.rs`'s loadCompleted arm (and every other Starfish/ACB
    // verb) must never observe "Load has returned" any later than this. Route tickets are a
    // SEPARATE mechanism (a ticket can be rejected as stale independent of this gate) and are
    // published after, not before, on purpose.
    if let Some(elapsed) = SHARED.mark_native_load_returned(native_epoch) {
        // Captured HERE, at the moment the flag actually flips, so the "native: Load returned
        // after Nms" log measures the real in-flight duration rather than however long it took
        // the main-thread pump to next run and notice. Logged unconditionally, right at the gate
        // transition, rather than from `pump.rs`'s `loadCompleted` arm — that arm only runs once
        // `loadCompleted` also arrives, so on a session where Load returns but `loadCompleted`
        // never does, the old placement left this line unlogged forever, with no way to tell
        // "Load is still on the stack" from "Load returned and the pipeline went quiet". See
        // finding `load-returned-log-is-conditional-on-loadcompleted-and-unbounded-after`.
        let elapsed_ms = elapsed.as_millis() as u64;
        SHARED
            .native_load_elapsed_ms
            .store(elapsed_ms, Ordering::Relaxed);
        super::log(&format!("native: Load returned after {elapsed_ms}ms"));
    }
    if let Some(ticket) = route_start {
        crate::playback::publish_route_start_result(
            ticket,
            if ok != 0 {
                crate::playback::RouteStartResult::Started
            } else {
                crate::playback::RouteStartResult::StartFailed
            },
        );
    }
    if ok == 0 {
        // Publish it. This used to be logged and discarded, so a refused payload was
        // indistinguishable from a slow one and the pump waited on a `loadCompleted` that could
        // never come.
        SHARED.publish_native_load_failure(native_epoch);
    }
}

