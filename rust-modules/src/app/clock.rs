//! The application's clock: ONE door to frame time. `now()` reads `SDL_GetTicks` — the only call
//! site of it in `app/` — so every read in a frame (the ingest stamp, each press arm, the HUD
//! linger) goes through one function. Main-thread only; a worker that needs wall time uses
//! `Instant`.

#[cfg(not(test))]
extern "C" {
    fn SDL_GetTicks() -> u32;
}

/// The host test binary links no SDL (`diag::heartbeat` carries the same stub for the
/// performance counter).
#[cfg(test)]
#[allow(non_snake_case)]
unsafe fn SDL_GetTicks() -> u32 {
    0
}

/// Milliseconds since SDL init.
pub(crate) fn now() -> u32 {
    // SAFETY: SDL is initialised before the loop that reads the clock exists; the call takes no
    // arguments and touches no memory of ours.
    unsafe { SDL_GetTicks() }
}
