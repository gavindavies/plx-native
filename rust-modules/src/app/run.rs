//! The frame loop — the THIN COORDINATOR. `run` is the one `while app.running` loop; each phase
//! of an iteration is a function below, named after the frame algorithm (`ui::dispatch`'s ten
//! steps). The positional frame points (NAV COMMIT, `opaque_route`, the present decision,
//! `clear_opaque_region` inside [`draw`], the swap) are the labelled lines of `run` itself, which
//! is why they are not folded into a phase function.
//!
//! The loop asks the dispatcher four questions and obeys its queues:
//!
//! * `bridge::owns_input` — asked by the key, pointer, click and wheel arms alike. When it is true
//!   the arm pushes an `InputEvent` onto `app.inputs` and returns; the ladders see nothing.
//! * `bridge::host_frozen` / `host_replaced` — the container's fold.
//! * `bridge::page_owned` — whether the dispatcher draws the top PAGE too, folded with
//!   `host_replaced` into `bridge::page_plan`.
//! * the requests an owned screen made of the loop (`content::content_requests`).
use super::*;

/// The per-iteration values that cross a phase boundary. Reset at the top of every iteration
/// (`Frame::begin`); every field is written by exactly one phase and read by the ones after it.
pub(crate) struct Frame {
    /// `clock::now()` at the ingest boundary — THE frame time every phase after it uses.
    pub(crate) now: u32,
    /// Seconds since the previous frame's `now`, clamped to 50 ms (the animation timestep).
    pub(crate) dt: f32,
    /// Something under a modal surface is still moving (its host must not freeze yet).
    pub(crate) underlay_moving: bool,
    /// The route is the player. NOT the present-gate question, which is
    /// `Player::video_plane_bound` (phase 9) — this is only "which screen is up", and the one
    /// thing still keyed on it is the draw's own `clear_opaque_region` + transparent clear.
    pub(crate) player: bool,
    /// This iteration draws and swaps (the present decision, spec §3.3 step 8).
    pub(crate) present: bool,
    /// This iteration did NOT present only because a page capture is still in flight on the GPU
    /// (`gfx::snapshot_pending`): the window was active, so the fence was the one reason. Pure
    /// bookkeeping for the present-interval stamps and the bench; it never feeds a decision.
    pub(crate) deferred: bool,
    /// The heartbeat's route word for this frame (`route_word`).
    pub(crate) rn: &'static str,
}

impl Frame {
    fn begin() -> Frame {
        Frame {
            now: 0,
            dt: 0.0,
            underlay_moving: false,
            player: false,
            present: false,
            deferred: false,
            rn: "",
        }
    }
}

/// The loop. `run_application` calls this once, after `boot`, and `shutdown` after it returns.
///
/// The main-thread token is a FIELD (`app.adapters.player`, see `player::adapter`), so the proof
/// travels with `&mut App` — every phase function below reaches the native session as
/// `&mut app.adapters.player`.
pub(crate) unsafe fn run(app: &mut App) {
    let watchdog = plx_base::task::watchdog::LoopWatch::start();
    while app.running {
        watchdog.advance();
        let _frame_scope = plx_base::task::FrameScope::enter();
        let wslg_frame_budget = app.wslg_frame_pacing.then(super::window_activity::WslgFrameBudget::begin);
        let mut fr = Frame::begin();
        let fr = &mut fr;
        app.instr.mark(plx_base::diag::heartbeat::Phase::Top);
        ingest(app, fr);
        app.instr.mark(plx_base::diag::heartbeat::Phase::Ingest); // ingest

        fr.now = clock::now();
        // Share the frame timestamp, before spring dt is clamped.
        plx_ui::card_motion::begin_frame(fr.now);
        // **The Player machine's tick**: set ONCE per iteration, from the same `fr.now` every
        // other phase of this frame reads.
        app.player.set_now(fr.now);
        crate::app::local_launch::tick(app, fr.now);
        crate::app::local_watch::tick(app, fr.now);
        plx_base::diag::spans::span("pump", || unsafe { playback_tick(app, fr) });
        clock_and_press(app, fr);
        plx_base::diag::spans::span("land", || unsafe { land_results(app, fr) });
        app.instr.mark(plx_base::diag::heartbeat::Phase::Results); // results
        // **Publish the playback session into the tree** before the dispatcher's frame and the
        // draw that follows it, so every owned screen in one frame reads one consistent picture.
        // Only while something that reads it is mounted — see `Bridge::publish_playback`.
        let was_player = super::bridge::player(&app.pages).is_some();
        // A timed-out native Load parked by teardown is released here, on the main thread, the
        // first frame after its media thread returns (`player::engine::AbandonedLoad`).
        plx_media::player::engine::reap_abandoned_load(&mut app.adapters.player);
        app.bridge.publish_playback(&app.player.session, was_player);
        // The tab bar's pills and strip, for the page the container is about to step.
        app.bridge.publish_tabs(&mut app.pages, fr.dt);
        // The container runs its frame: the pending navigation's commit (at `PageDip`'s floor),
        // then the owned screens' inputs, ticks, timers and effects. `app/bridge.rs` is the seam.
        let (_word, tree_report) = super::bridge::frame_with_tap(
            &mut app.pages,
            &mut app.bridge,
            plx_machine::machine::Tick {
                ms: fr.now,
                dt_us: (fr.dt * 1_000_000.0) as u32,
            },
            std::mem::take(&mut app.inputs),
            &mut plx_ui::dispatch::NoTap,
        );
        // **The OWNED pages' share of the underlay verdict**: the dispatcher keeps it on its own
        // ledger (`Present::page_moving`), and `popover::host::begin_frame` reads it to decide
        // whether the frozen snapshot under a FADING panel is taken again.
        fr.underlay_moving |= tree_report.underlay_moving;
        content_requests(app, fr);
        app.instr.mark(plx_base::diag::heartbeat::Phase::NavCommit); // navcommit
        update(fr);
        app.instr.mark(plx_base::diag::heartbeat::Phase::TickDrain); // tick_drain
        prepare_window(app, fr);
        present_and_swap(app, fr, wslg_frame_budget);
        if fr.present { watchdog.presented(); }
        report(app, fr);
        heartbeat(app, fr);
    }
}

/// **The frame's clock and the click's spring** (spec §3.3 step 3's head).
///
/// `dt` is clamped at 50 ms: a frame that took longer than that is a stall, and integrating one
/// as if it were real time teleports every spring. It is stamped into `plx_machine::idle::frame_begin`
/// BEFORE the update phase re-steps anything, so the motion flag the gate reads at the bottom
/// describes THIS frame and a spring's velocity can be judged as travel-this-frame rather than as
/// a bare units-per-second.
fn clock_and_press(app: &mut App, fr: &mut Frame) {
    fr.dt = {
        let mut d = if app.prev != 0 {
            fr.now.wrapping_sub(app.prev) as f32 / 1000.0
        } else {
            0.016
        };
        if d > 0.05 {
            d = 0.05;
        }
        d
    };
    app.prev = fr.now;
    // Whole-frame present gate (`plx_machine::idle`): forget last frame's motion BEFORE the update
    // phase below re-steps every spring, so the flag it leaves describes THIS frame, and
    // stamp `dt` so a spring's velocity can be judged as travel-this-frame rather than as
    // a bare units-per-second. The decision itself is taken just above `glViewport`.
    let rest_dt = fr.dt;
    plx_machine::idle::frame_begin_judging_rest_with(fr.dt, rest_dt);
    // Is a page capture still in flight on the GPU, and is text still warming? Latched once,
    // before the springs step, so the held appear spring and the present gate below read the same
    // answer.
    let text = plx_gfx::text::prewarm_pending();
    plx_gfx::gfx::snapshot_frame_begin(|snapshot| {
        plx_gfx::text::latch_surface_text_pending(text);
        snapshot
    });
    // ui::press (tvOS click) — advance the dip/spring every frame; when a deferred activation
    // commits (the spring-back bounce has played), run it for whichever CARD view armed the
    // press. A long-press does NOT commit (`press::tick` clears `want_commit` at `LONG_MS`):
    // on Home it opens the item menu below, and anywhere else it just springs back.
    let (_, press_moving) = plx_machine::idle::scoped_motion(|| {
        app.input.press.tick(fr.now, fr.dt);
    });
    // The motion of whatever page is UNDER a popover — Home, the Library or Search, since
    // the profile chip is a stop on all three. It was `home_underlay_moving` while only Home
    // could be underneath. The account popover's glass re-snapshots off this.
    fr.underlay_moving = press_moving;
}

/// **The prepare window** (spec §3.3 steps 8-9, §8.1): the frame budget's own frame, the poster
/// adapter's, the player page's clock-motion report, the whole-frame present DECISION, and — on
/// the presenting side of it — the render cache's upload.
unsafe fn prepare_window(app: &mut App, fr: &mut Frame) {
    // ---- the PREPARE WINDOW opens here (spec §3.3 steps 8-9, §8.1) -------------------
    //
    // **The budget's frame opens at the start of this window, not at the top of the
    // iteration**, and that placement is the whole of its ceiling's meaning. `take` admits
    // while `now - frame_start + worst_us <= PREPARE_MAX_US`; opened at the loop top, the
    // elapsed term already contained ingest, the result landings, the nav commit and the
    // tick drain, so on a cold-open frame (~62 ms) every upload was over the ceiling and
    // only the forward-progress escape let ONE through. A quota of three that was really a
    // quota of one, on exactly the frames with the most textures waiting.
    app.pages.budget.begin_frame(plx_base::diag::heartbeat::now_us());
    // The poster adapter's frame (spec §3.3 step 3's tail): a new frame for the slot LRU and
    // every decoded image handed to the render cache as owned pixels. No GL here — the
    // upload is below, on the presenting side of the decision.
    super::adapters::poster::begin_frame();
    super::adapters::poster::drain_decoded();

    fr.player = matches!(app.route(), AppArg::Player);
    // ---- the player page's CLOCK-DRIVEN motion report (spec §8.3, §9) ----
    //
    // After the update phase, so this frame's springs and this frame's pump have been seen,
    // and before the gate below reads the verdict. Everything the player page draws from a
    // clock rather than from a spring is folded into ONE value and compared with last frame's;
    // `PlayerScreen::clock_fingerprint` is the inventory and the argument. Nothing to do off
    // the route — the page is not mounted and nothing it owns is on screen.
    if fr.player {
        let fp = super::bridge::player(&app.pages)
            .map(|p| p.clock_fingerprint(&app.player.session, fr.now));
        if let Some(fp) = fp {
            if app.player.note_clock(fp) {
                plx_machine::idle::invalidate();
            }
        }
    }
    // ---- whole-frame present gate (`plx_machine::idle`) --------------------------------------
    // A screen with nothing moving on it does not need to be re-sent to the panel. This
    // skips `glViewport`…`SDL_GL_SwapWindow` WHOLESALE — it is not dirty-RECTANGLE
    // tracking, which `plx_ui`'s `lib.rs` renderer doc rejects: when this says yes, the frame
    // below is byte-for-byte the immediate-mode frame it always was, clear and all.
    //
    // Measured cost of not doing this (2026-07-31): a still Home grid burns 16.0% of one
    // A53 core here plus ~19.4 points inside `surface-manager`, which must blend our
    // 1080p surface on every present — a charge that measured identical on three
    // different screens, so it is per-PRESENT, not per-pixel. ~35 points of a core to
    // re-send an unchanged picture, on a fan-less SoC that sits on Home for hours.
    //
    // **The exclusion is the BOUND PLANE, not the player ROUTE** (spec §9, phase 9).
    // `system.rs::clear_opaque_region` documents the hardware video plane as *slaved* to this
    // wayland surface, and "we stop presenting while a plane is slaved to it" is a claim about
    // this compositor that reading cannot settle — so while the plane is bound the gate
    // answers `true` unconditionally, and that term now lives INSIDE `should_present`, fed by
    // `Player::set_video_plane_bound`'s edges and by nothing else.
    //
    // What that changes is the frames on either side of a playback. Pre-bind (the spinner
    // while the route resolves and the Load is in flight) and post-unbind (the HUD fading out,
    // the failure read-out) are ORDINARY idle frames: nothing is slaved to the surface, so the
    // 35-points-of-a-core argument applies to them exactly as it does to Home — and every
    // player-side animator that advances from a clock has to report, or it freezes. Playback
    // itself still spends ~99% of its time with the HUD auto-hidden, where the frame is
    // already 0 draw calls.
    //
    // **The foreground demand is taken ONCE and has TWO terms** (spec §3.3 step 8): the
    // gate above, and whether the frame budget holds queued prepare work. The second term is what makes
    // the upload step below legal on the presenting side of the decision — a texture waiting
    // to be uploaded is itself a reason to present, so nothing sits in the queue behind a
    // settled screen. It was inert before phase 11: nothing in the product ever published a
    // queue to the budget, so `has_queued_work()` was permanently false and the upload had to
    // run BEFORE the decision (and invalidate) to happen at all.
    plx_ui::tex::note_queued(&mut app.pages.budget);
    // Lifecycle is a hard outer gate: even noidle, a bound plane or queued uploads must
    // not reach EGL/Mali while SDL's window is backgrounded.
    //
    // A third term goes FIRST and short-circuits the other two: a page capture still in flight on
    // the GPU (`gfx::SNAPSHOT_THIS_FRAME`'s doc). Presenting now would only wait a whole vsync for
    // a buffer; not asking `should_present` leaves its damage and motion for the frame that does.
    // The simulator's settled capture (`PITV_SHOT_SETTLE`): once nothing has changed for the
    // asked-for quiet, invalidate so the NEXT present is the settled frame and `maybe_capture`
    // takes it. Before the decision below, so that invalidate selects this very frame.
    crate::shot::tick(fr.now, app.pages.budget.has_queued_work() || plx_gfx::gfx::snapshot_pending());
    fr.present = !plx_gfx::gfx::snapshot_pending()
        && app.window_activity.allow_present(
            plx_machine::idle::should_present(fr.now) || app.pages.budget.has_queued_work(),
        );
    // Read-only: was the fence the only thing standing between this iteration and a present? An
    // inactive window would not have presented either way. (`should_present` is deliberately not
    // asked: it takes-and-clears the damage the deferred frame still needs.)
    fr.deferred = !fr.present && plx_gfx::gfx::snapshot_pending() && app.window_activity.allow_present(true);
    // ---- step 9's UPLOAD, on the presenting side of the decision ---------------------
    //
    // The render cache's upload step under the frame budget (§3.3 step 9, §10: "a frame that
    // does not present uploads nothing"). Two placement rules, both load-bearing:
    //
    // * **Only on a presenting frame.** The spec's rule: a frame that is skipped wholesale
    //   uploads nothing and the queue waits (`GfxUploader::warm` is `gfx::warm_tex`, which
    //   also draws, so it wants the frame's GL state in place).
    // * **Before the draw.** `warm_tex` draws into its own tiny offscreen target and never
    //   touches framebuffer 0 (the first framebuffer-0 draw of a frame is where the back-buffer
    //   wait lands, and it must stay with the page's `frame_clear`, not with this window), so
    //   there is no pixel to show over the picture — film included — wherever it runs; it stays
    //   here so the upload's cost is bounded by the budget and off the draw.
    if fr.present {
        // A Tracks/More page's text, recorded in `update` or, on the mount frame, in the overlay's
        // `prepare` (`PanelMotion::prewarm_text`), and uploaded here: `fr.present` already carries the window-activity gate, so a frame that
        // does not present, or one while the window is backgrounded, uploads nothing.
        plx_ui::panel_motion::PanelMotion::drain_queued_text(plx_base::diag::heartbeat::now_us);
        // The player's UltraBlur envelope, latched while nothing is open so the first Tracks/More
        // popover's open frame does not pay for it (`ModalUnderlay::preload`; an upload, hence here).
        app.pages.nav.modals.preload_underlay();
        let mut ph = plx_machine::machine::PresentHandle::of(&mut app.present);
        super::adapters::poster::prepare(
            &mut app.pages.budget,
            &mut ph,
            plx_base::diag::heartbeat::now_us,
        );
    }
    app.instr.mark(plx_base::diag::heartbeat::Phase::Prepare); // prepare
    // `worstprep=`: the prepare phase is timed on EVERY iteration, presented or not — a
    // settled screen must never run untimed work at the loop rate (spec §8.3).
    app.instr.note_prepare();
    // Hoisted: the frame-drop detector reads these after the gate. Seeded to the pump
    // stamp so a skipped frame reports zero draw/cap/swap rather than a stale delta.
    app.instr.seed_present_phases();
}

/// One idle iteration's font-warming slice: a key pressed during it waits at most this long more
/// than the idle poll already makes it wait.
const FONT_WARM_SLICE_US: u64 = 2_000;

/// **Draw, capture, swap — or sleep one frame period** (spec §3.3 step 10). Everything in here is
/// inside the present gate's decision, which `prepare_window` has already taken into `fr.present`.
unsafe fn present_and_swap(
    app: &mut App,
    fr: &mut Frame,
    wslg_frame_budget: Option<super::window_activity::WslgFrameBudget>,
) {
    if fr.present {
        // the glyph cache's frame serial (phase 11, text.rs's hot window): a drawn frame
        plx_gfx::text::begin_frame();
        // A finished underlay-field reduction is read HERE, before framebuffer 0 holds anything
        // of this frame — a read between the page and the surfaces splits its render pass.
        plx_gfx::gfx::field_frame_begin();
        let (_vx, _vy, _vw, _vh) = draw(app, fr);
        app.instr.mark(plx_base::diag::heartbeat::Phase::Draw); // draw
        app.instr.mark(plx_base::diag::heartbeat::Phase::Capture); // capture
        // The decoded picture goes UNDER the finished UI (`player::gst_sink`) — before the
        // capture, so screenshots include it.
        if fr.player {
            plx_media::player::gst_sink::composite_under();
        }
        // Before the swap, never after: the back buffer is undefined once presented.
        if crate::shot::maybe_capture(_vx, _vy, _vw, _vh) {
            // The headless one-shot is done: finish this frame, then leave through the ordinary
            // shutdown rather than exiting from inside the frame (see `shot::maybe_capture`).
            app.running = false;
        }
        {
            #[cfg(feature = "threadcheck")]
            let _present_scope = plx_base::task::watchdog::present_scope();
            plx_base::surface::present_supersampled();
            SDL_GL_SwapWindow(app.win);
        }
        // The swap's completion is a point on the vblank grid the next picture is chosen against.
        if fr.player {
            plx_media::player::gst_sink::swapped();
        }
        app.window_activity.presented();
        #[cfg(feature = "devtools")]
        {
            app.buffer_flip_count = (app.buffer_flip_count + 1) % 60;
        }
        app.instr.mark(plx_base::diag::heartbeat::Phase::Swap); // swap
        plx_gfx::overdraw::frame_end();
        // Same reason, same gate: the blur's region accounting is per DRAWN frame. It rolls
        // "what every glass surface asked for this frame" into the region the next frame's
        // first snapshot is taken at. Once that union is known, several surfaces share one
        // capture; a first discovery frame may still need a second non-contained grab.
        plx_gfx::gfx::blur_frame_end();
        app.glass.sources.borrow_mut().finish();
        // …and a queued underlay-field reduction has had one more drawn frame to finish in.
        plx_gfx::gfx::field_frame_end();
        // …and a frame that captured the page leaves a fence the next frames wait on.
        plx_gfx::gfx::snapshot_frame_end();
        // …and a ground probe's queued copy is read back here, between frames, if it is done.
        plx_gfx::gfx::ground_probes_frame_end();
        plx_machine::idle::note_present(fr.now);
        // The poster-gate scenes' frame counter, at the same post-swap seam as the present count
        // above. It was called from inside `note_present`; `plx_machine::idle` is the machine layer and
        // no longer names `ui`'s metrics.
        if let Some(budget) = wslg_frame_budget {
            budget.finish();
        }
    } else {
        if fr.deferred {
            // The fence held a present back: the panel kept the old picture for this iteration, so
            // the next presented frame's interval must span it. An idle gap breaks the cadence; a
            // deferral is part of it.
            app.instr.defer_present_phases();
        } else {
            app.instr.skip_present_phases();
        }
        // The one stretch of main-thread time nothing is waiting on: spend a bounded slice of it
        // opening the theme faces and loading their glyph metrics (`text::warm_fonts_idle`), so
        // a page's first layout does not pay for them inside a transition. A no-op once warm.
        plx_gfx::text::warm_fonts_idle(FONT_WARM_SLICE_US, plx_base::diag::heartbeat::now_us);
        // Device and macOS presented frames block in swap; WSLg/X11 presented frames use the
        // software budget above. A skipped frame reaches neither path, so sleep here to keep a
        // settled screen from becoming a CPU spinner.
        SDL_Delay(plx_machine::idle::IDLE_POLL_MS);
    }
}

/// Ingest: the remote FIFO and the SDL event queue (spec §3.3 step 2). Every key, pointer and
/// lifecycle event the frame acts on enters here.
pub(crate) unsafe fn ingest(app: &mut App, fr: &mut Frame) {
    let mut toks: Vec<String> = Vec::new();
    if let Some(r) = app.remote.as_mut() {
        r.drain(|tok| toks.push(tok.to_string()));
    }
    for tok in toks {
        let _ = ingress_token(app, fr, &tok);
    }
    drain_sdl(app, fr);
}

unsafe fn drain_sdl(app: &mut App, fr: &mut Frame) {
    while SDL_PollEvent(app.ev.as_mut_ptr() as *mut c_void) != 0 {
        ingest_sdl_event(app, fr);
    }
}

unsafe fn ingress_token(app: &mut App, fr: &mut Frame, token: &str) -> bool {
    #[cfg(feature = "devtriggers")]
    if let Some(probe) = crate::remote::HangProbe::parse(token) {
        // Earlier keys/clicks may still be in SDL: handle them before the stall.
        drain_sdl(app, fr);
        probe.run();
        return true;
    }
    let _ = (app, fr);
    dispatch_remote_token(token)
}

/// One polled event.
unsafe fn ingest_sdl_event(app: &mut App, _fr: &mut Frame) {
    let et = rd_u32(&app.ev, 0);
    app.window_activity.event(et);
    // INPUT while a popover holds the page frozen is the POPOVER's: every invalidate
    // such an event raises — the one below, and whatever its handler adds — is
    // attributed to it, so the frozen host is not re-rendered on every key-up
    // (`popover::host::input_scope`). Only input: a lifecycle or window event is the
    // APP's and may change the page under the panel (backgrounding drops Search's
    // editing layout), so its damage stays the page's and the snapshot is retaken.
    let _own_input = if is_input_event(et) {
        plx_ui::popover::host::input_scope()
    } else {
        None
    };
    // ANY event is a reason to repaint (`plx_machine::idle`): a key changes focus or a label,
    // a lifecycle event changes the whole screen. Marked here — once, for every event
    // kind — rather than in each of the ~30 arms below, where the next one added would
    // silently draw nothing.
    plx_machine::idle::invalidate();
    if et == SDL_KEYDOWN
        || et == SDL_KEYUP
        || et == SDL_TEXTINPUT
        || et == SDL_TEXTEDITING
    {
        // 48 bytes, not 32: a TEXTINPUT event's `text[32]` starts at +16 on the
        // television (LG's `inputSource` shifts it), so it ENDS at exactly +48. At the
        // old width the one event whose payload the offsets are most easily wrong
        // about would have left a forensic trail that stopped just before the payload.
        let mut hex = String::with_capacity(96);
        for b in &app.ev[..48] {
            hex.push_str(&format!("{b:02x}"));
        }
        let what = match et {
            SDL_TEXTINPUT => "text",
            // **The IME's PRE-EDIT, and the reason it is logged at all.** The panel's
            // word prediction is a REPLACE — tapping "summer" under a typed "summ"
            // means *delete what I was predicting on, then commit this* — and the app
            // sees only the commit, so the field reads "summsummer" (reported from the
            // couch 2026-08-15). Whether the delete half reaches us as `SDL_TEXTEDITING`
            // (`text_model.delete_surrounding_text` mapped onto SDL's pre-edit) or as
            // nothing at all decides whether the fix can be exact or has to be a
            // heuristic — and this arm is the only way to find out, since nothing in
            // the app has ever read this event.
            SDL_TEXTEDITING => "edit",
            _ => "key",
        };
        log(&format!(
            "[{}] {what} type=0x{et:x} raw={hex}",
            clock::now()
        ));
    }
    if et == SDL_QUIT {
        app.running = false;
    } else if et == 0x103 || et == 0x104 {
        // WILL/DID ENTER BACKGROUND. A Linux desktop does not take the screen away
        // from a running app, so playback carries on; the TREE still hears it, as
        // `ScreenEvent::Suspend` down every mounted body at this frame's NAV COMMIT.
        log("LIFECYCLE: background");
        super::bridge::background(&mut app.pages);
    } else if et == 0x105 || et == 0x106 {
        // WILL/DID ENTER FOREGROUND: the other half, on both edges (`Navigation::resume` is
        // idempotent, so the pair is safe and a lost one is not).
        log("LIFECYCLE: foreground");
        super::bridge::foreground(&mut app.pages);
        plx_machine::idle::invalidate();
    } else if et == SDL_KEYDOWN || et == SDL_KEYUP {
        let (state, wcode, sym) = decode_key(&app.ev);
        // The press's IDENTITY, resolved once from the two raw fields
        // (`ui::consts::classify`, which is where the spellings live and where they
        // are tested). `sym` and `wcode` are still read raw by the arms below — the
        // ones that forward them to a screen's own `move_focus`/`key`, and the modal
        // panels and the CH▲/CH▼ pager, which still spell their own key tests.
        let key = classify(sym, wcode);
        // **Does the TREE own this key?** (phase 5b, `app/bridge.rs`'s coexistence
        // contract.) Asked once, here, above all three edges — because the dispatcher's
        // press machine wants ALL of them: `Edge::Down` arms, `Edge::Repeat` is the
        // dropped-key-up net's liveness beat, and `Edge::Up` is the release. Handing it
        // only the fresh presses would leave a dipped control committing a second later
        // at `press::MAX_HOLD_MS` instead of on the button coming up.
        let tree_owns_key = super::bridge::owns_input(&app.pages);
        // `clock::now()`, not `fr.now`: INGEST runs before the frame stamps its own time
        // (see `run`'s phase order), so `fr.now` is still 0 here — the same reason the
        // ladder below reaches for the clock to fill `app.last_input`. `dt_us` is 0
        // because an event's `at` is a stamp, not a timestep: every machine the
        // dispatcher steps is driven by the FRAME's tick, which `bridge::frame` supplies.
        let tree_tick = plx_machine::machine::Tick {
            ms: clock::now(),
            dt_us: 0,
        };
        if (state & 0xff) != 1 {
            if tree_owns_key {
                app.inputs.push(super::bridge::key_input(sym, wcode, state, tree_tick, plx_machine::machine::Source::Sdl));
            }
            // …and the loop's own key-up bookkeeping runs either way: it retires the sym
            // from the physically-down slot and releases a deferred press, both of which are
            // state about the PHYSICAL key rather than about whoever read it. Skipping it
            // while a surface is up would leave a key the ladders never saw go down looking
            // held the moment the surface closes.
            //
            // **It no longer touches the scrub** (restructure phase 12, PX-PLAYER). It used to
            // commit, cancel or debounce `PlayerScreen::scrub` from here — a field the page
            // itself has owned since it began answering `HitSource::Engine`, and which arms its
            // own `TAP_COMMIT_MS` debounce on the very same `Edge::Up` — so one tap of RIGHT
            // issued two seeks. The publish that followed went with it: nothing here moves a
            // published field any more.
            on_key_up(sym, app.ok_armed, &mut app.down_sym, &mut app.input.press);
            return;
        }
        // A repeat is only a repeat if we watched the key go down. See `App::down_sym`:
        // the system keyboard eats key-ups, so the driver stamps 0x100 on presses that are
        // the FIRST of their own gesture, and dropping those loses one press in two.
        if state & 0x100 != 0 && sym == app.down_sym {
            if tree_owns_key {
                // **Item 13 survives the migration, and only the DIRECTIONS are gated.**
                // A held key repeats every ~50 ms; a settings row or a line of reading
                // text per beat is a blur nobody can track, and the focus engine has no
                // cadence of its own — so `RepeatGate` still throttles the moves. The OK
                // edges go through UNGATED, deliberately: `dispatch`'s ingest reads an OK
                // `Edge::Repeat` as `press.note_alive`, so a swallowed beat is a hold that
                // looks like a lost key-up and springs back without activating.
                let ok = is_ok(sym);
                if ok || app.modal_repeat.ready(tree_tick.ms) {
                    app.inputs.push(super::bridge::key_input(sym, wcode, state, tree_tick, plx_machine::machine::Source::Sdl));
                }
                return;
            }
            // All that is left of the legacy repeat path is the deferred press's liveness
            // beat: the player's continuous scrub, its last direct consumer, engages from
            // `PlayerScreen`'s own `Edge::Repeat` (phase 12, PX-PLAYER).
            on_auto_repeat(sym, app.ok_armed, &mut app.input.press);
            return;
        }
        // From here down this IS a fresh press, whatever the driver stamped on it.
        app.last_input = clock::now();
        begin_fresh_press(&mut app.player.session, 
            key,
            sym,
            wcode,
            app.last_input,
            &mut app.down_sym,
            super::bridge::player_mut(&mut app.pages).map(|player| &mut player.hud),
            &mut app.ptr,
            &mut app.ok_armed,
            &mut app.input.press,
        );
        // `note_fresh_press` may have extended the auto-hide deadline; publish it once, here.
        if let Some(player) = super::bridge::player_mut(&mut app.pages) {
            player.publish();
        }


        // ---- the route-scoped arms, each of which `continue`s once it has taken the
        // press. That makes the chain itself the priority statement: an earlier guard
        // subsumes each later one it overlaps with, which the playback-failure guard
        // below does deliberately. Keep it a chain — a `match` over the same routes
        // compiles and keeps the suite green while silently reordering it, because
        // exhaustiveness cannot see subsumption.
        //
        // **THE DISPATCHER'S ARM, and it is one line because that is the whole point**
        // (phase 5b). Three hand-written arms stood here — consent above legal above the
        // settings root — and the height of each in this chain WAS its modality: each
        // `continue`d on every key, and the ordering was load-bearing because BACK out of
        // the privacy notice would otherwise have been read as Home's ROOT PRESS and
        // handed the screen to the television. That ordering is now a container's: the
        // Settings family is a modal SURFACE on the app's `ModalStack`, whose inner
        // `NavStack` walks itself on BACK and only then lets the container dismiss it, so
        // "which of the four answers this key" is a tree walk rather than a chain the next
        // editor has to keep in order. `owns_input` is the same question the pointer,
        // click and wheel arms below ask, which is what stops the four drifting apart.
        //
        // It stays HIGH in the chain for the reason the old arms did: an owned surface is
        // modal over whatever route is behind it, and every route arm below would
        // otherwise act on a page the user cannot see.
        //
        // **The fourth root is closed here** (`app/input.rs`'s `back_at_root`). BACK at
        // the FIRST consent stage used to be swallowed — the step behind it is sign-in,
        // which cannot be undone — and the old comment recorded why the 2026-09-03 root
        // rule could not reach it: `ui::consent::on_back` reported `true` for both the
        // stepped-back and the swallowed case, so a BACK arm could not tell them apart
        // without changing that module's return type. The owned screen simply says which
        // it is: `ConsentPage` answers `Handled::No` at a Settings BACK and pushes
        // `LoopReq::BackAtRoot` at the first stage, and the request drain below performs
        // it. Nothing is stranded by going to the television's Home — the question is
        // neither answered nor dismissed, and selecting the tile again comes back to it.
        if tree_owns_key {
            app.inputs.push(super::bridge::key_input(sym, wcode, state, tree_tick, plx_machine::machine::Source::Sdl));
            return;
        }
        // `Route::Login | Route::Profiles` is deliberately absent here (phase 6, mirroring
        // `Route::Onboard`'s own removal in 5b): both are OWNED screens now, so a key on
        // either was already taken by `tree_owns_key` above and never reaches this chain.
        // (`Route::ItemMenu`'s arm stood here, above every route-scoped one — it was MODAL, so it
        // took every key. The item menu is a `ModalStack` surface since phase 10, so its keys were
        // already claimed by `tree_owns_key` well above this chain and it can never be reached.)
        // (The failure read-out's own key arm stood here — OK to the quality ladder, BACK out of
        // the playback, everything else swallowed, and its guard deliberately subsuming every
        // player arm below it. It was already unreachable when phase 12 found it, having grown a
        // `!matches!(app.route, Route::Player)` term that can never hold beside the
        // `Route::Player` test above it; what it was FOR is `PlayerScreen::handle_key`'s own
        // `transport_hidden` gate, which asks `failed_key_action` — the same pure policy — before
        // any ordinary transport arm.)
        // (Four arms stood here — the track menu, the `…` popover, the Info card and the
        // Chapters strip, each gated on `overlay_swallows_key` so that a transport key FELL
        // THROUGH to the ordinary Pause/Play arms below while everything else was swallowed. The
        // panels are SURFACES on the player page's own `ModalStack` since restructure phase
        // 9, so `tree_owns_key` at the top of this chain has already handed the key to the one
        // that is open and returned; the fall-through, which a surface cannot perform, is
        // `PlayerReq::Transport` — see `screens::player::overlay`'s module doc.)
        // (The player's UP/DOWN arm stood here. It walked the HUD ring on the mounted
        // `PlayerScreen`'s own `hud`/`scrub` — which the page also does, from `key_updown`, for the
        // same key — and was unreachable behind `tree_owns_key` above from the moment the page
        // answered `FocusSource::Engine`. Deleted in phase 12, PX-PLAYER.)
        // `Route::Search` is deliberately absent here (phase 6/7 Search cutover, mirroring
        // `Route::Login | Route::Profiles` above): Search is unconditionally an OWNED screen
        // now, so `tree_owns_key` above already took every key on this route and returned —
        // this chain can never see one.
        // ---- and the arms on key IDENTITY, which the routes above have already had
        // their pick of. Still one `else if` chain, still in this order: four of its
        // nine tests carry a route term as well as a key one (this first arm, Stop, the
        // player's LEFT/RIGHT and the Library pager), so the order is behaviour too.
        //
        // The plain syms only (`alt: false`) — the alternate D-pad codes reach no arm
        // that navigates a non-player screen. See `Key::Left`, which carries that
        // asymmetry between this test and the player's scrub arm below.
        if !matches!(app.route(), AppArg::Player)
            && matches!(
                key,
                Key::Up
                    | Key::Down
                    | Key::Left { alt: false }
                    | Key::Right { alt: false }
            )
        {
            // Every non-player route reaching this far down the ladder is an OWNED page
            // (Home/Library/Search included, since the Search cutover), and an owned page's
            // directions are taken by `tree_owns_key`'s arm well above this chain — this branch
            // is reached by nothing and does nothing (`key_move_focus`, the function that used
            // to live here, was itself already an unconditional no-op and is retired). The
            // condition stays so this arm keeps its PRECEDENCE over `wcode == WCODE_POINTER_HIDDEN`
            // below for the documented direction+wcode-hidden combination noted there; removing
            // the branch instead of its body would let that combination fall through to it.
        } else if wcode == WCODE_POINTER_HIDDEN {
            // LG pointer auto-hidden; ignore.
            //
            // THE RAW `wcode`, not `Key::PointerHidden`, and this is the one arm in the
            // ladder that cannot use the classified value. Its precedence here is
            // ROUTE-DEPENDENT: the nav arm above it is `!Player && <direction>`, so for
            // an event carrying BOTH a direction sym and this wcode, Home moves focus
            // (the nav arm wins, being higher) while the player swallows it (the nav arm
            // is skipped, and this one catches it before the scrub arm below).
            //
            // `classify` is a pure function of the pair and cannot express that — it has
            // one linear order and no route. Ordering it directions-first reproduces
            // Home and makes the player SEEK on a pointer notification; ordering it
            // pointer-first reproduces the player and freezes Home's navigation. So the
            // classifier keeps directions first (Home correct) and the raw test stays
            // here, at the position that was always the player's answer.
            //
            // Whether any real event carries that pair is unrecorded: nothing in the
            // tree names the sym beside wcode 0x1e4, `remote_token_key` never emits it,
            // and the simulator cannot produce one — so `tools/keytable.py` is blind to
            // this by construction. Settling it needs a `key` line off the television.
        } else if matches!(key, Key::Ok) {
            key_ok(&mut app.adapters.player,
                app.last_input,
                &mut app.ptr,
                &mut app.ok_armed,
                &mut app.input.press,
                &mut app.pages,
            );
        } else if matches!(key, Key::Pause) {
            key_pause(&mut app.adapters.player, app.last_input, &mut app.pages);
        } else if matches!(key, Key::Play) {
            key_play(&mut app.player.session,
                &mut app.adapters.player,
                app.last_input,
                &mut app.ptr,
                &mut app.pages,
                &mut app.bridge,
            );
        } else if matches!(key, Key::PlayPause) {
            // ONE key, both directions. `key_play`/`key_pause` are each half of the
            // toggle, so this arm picks; off the player route `key_play` is what starts
            // playback, which is the right answer for a PLAYPAUSE press on a card.
            if paused() || super::bridge::player(&app.pages).is_none() {
                key_play(&mut app.player.session,
                    &mut app.adapters.player,
                    app.last_input,
                    &mut app.ptr,
                    &mut app.pages,
                    &mut app.bridge,
                );
            } else {
                key_pause(&mut app.adapters.player, app.last_input, &mut app.pages);
            }
        } else if matches!(key, Key::Exit) {
            // The remote's EXIT key — LG's checklist item 38 wants the app terminated,
            // and unlike BACK at Home's root there is nothing ambiguous about a key
            // labelled EXIT, so unlike BACK it really does end the process — and it
            // is now the only key that does.
            log("EXIT key: terminating");
            app.running = false;
        // (Two player arms stood here — STOP's `exit_player`, and LEFT/RIGHT's `key_scrub`. Both
        // are `PlayerScreen`'s: `Key::Stop` asks for `PlayerReq::Exit` and reaches the very same
        // ritual through `player_requests`, and the scrub ladder is the page's three edges. Their
        // route terms were two of the four that made this chain's ORDER behaviour, which is why
        // they are recorded rather than silently dropped. Phase 12, PX-PLAYER.)
        } else if matches!(key, Key::Back) {
            key_back(&mut app.player.session,
                &mut app.adapters.player,
                &mut app.pages,
            );
        }
    } else if et == SDL_MOUSEMOTION {
        app.last_input = clock::now();
        app.ptr.last_motion = app.last_input;
        app.ptr.cur_hidden = false;
        let (mx, my) = ptr_xy(&app.ev);
        if app.ptr.prev_mx >= 0.0 {
            app.ptr.mot_accum += (mx - app.ptr.prev_mx).abs() + (my - app.ptr.prev_my).abs();
        }
        app.ptr.prev_mx = mx;
        app.ptr.prev_my = my;
        // **The BARE transport's pointer, handed to the page that owns it** (phase 12,
        // PX-PLAYER). This arm used to raise the HUD and move the scrub preview HERE, on the
        // screen's own fields, and then `return` — so the player page never saw a pointer event
        // at all, and the drag preview kept reading a flag whose producer had become unreachable.
        // Both halves are `PlayerScreen`'s now; what is left is the choice of EVENT.
        //
        // It stays AHEAD of the D-pad accumulation gate below, deliberately and as it always was:
        // over full-screen video a mouse nudge is how the transport is found, and 120 px of
        // accumulated travel is not what that costs. (`ui::hit`'s own dpad gate still suppresses
        // hover-driven focus underneath, which is the half that rule is actually about.)
        //
        // A button held down makes this a DRAG rather than a move — §7.5, the one pointer kind
        // that drives a control with no hover and no click.
        if matches!(app.route(), AppArg::Player)
            && !app.pages.surface_up()
            && super::bridge::owns_input(&app.pages)
        {
            let at = plx_machine::machine::Tick { ms: app.last_input, dt_us: 0 };
            app.inputs.push(if app.ptr.button_down {
                super::bridge::drag_input(mx, my, at)
            } else {
                super::bridge::pointer_input(mx, my, at)
            });
            return;
        }
        if app.ptr.dpad_mode {
            if app.ptr.mot_accum < 120.0 {
                return;
            }
            app.ptr.dpad_mode = false;
        }
        // Hover is the tree's on every screen it owns — `ui::route_screen`'s rule 11
        // ("hover parks focus on every screen in the family") is the HIT MAP's job now.
        // Three hand-written arms stood here, keyed by the same open-state flags the key
        // ladder used, and they existed because a hover ladder keyed by `route` reached
        // none of the family: a pointer move across Settings, Privacy & data or Legal
        // drove HOME's focus underneath instead. The press-cancel each of them paid for
        // by hand — a pointer sliding off the control it armed must abandon that press —
        // is `dispatch`'s ingest, which cancels an arm whose hit no longer resolves to it.
        if super::bridge::owns_input(&app.pages) {
            // `app.last_input`, stamped from the clock at the top of this arm: `fr.now`
            // is not written until after ingest (`tree_tick` above says why).
            app.inputs.push(super::bridge::pointer_input(
                mx,
                my,
                plx_machine::machine::Tick { ms: app.last_input, dt_us: 0 },
            ));
            return;
        }
        // `Route::Profiles | Route::Search | Route::ItemMenu` are deliberately absent (phases
        // 6/7/10): all three are owned screens or surfaces now, so their hover was already taken
        // by `owns_input()` above.
    } else if et == SDL_MOUSEBUTTONDOWN {
        app.last_input = clock::now();
        // The button is DOWN from here until its up, which is the whole of what the pointer
        // machine now records about it: motion in that window is a DRAG (§7.5) rather than a
        // move, and the idle-cursor hide below leaves a held pointer alone.
        app.ptr.button_down = true;
        // A FRESH click supersedes a press still in flight from the previous one — the
        // pointer's twin of `begin_fresh_press`'s nav-key abort. Without it, clicking a
        // control and then something else inside the ~210 ms commit window let the first
        // click's deferred activation fire AFTER the second had already acted (two
        // `on_ok`s: the watched toggle flipped twice, each with its own blocking
        // refetch). Every arm below re-arms from scratch.
        //
        // It sits at the TOP of the pointer handler rather than inside one route's arm,
        // where it lived while only detail cards armed a press from a click. Every
        // control face defers now, so the interleaving it guards is reachable on every
        // screen — including across arms, e.g. Home's hero pill pressed and then a tab
        // pill clicked, which navigates at once and would otherwise have played the
        // hero a moment later on the page it had just left.
        if app.ok_armed {
            app.input.press.cancel();
            app.ok_armed = false;
        }
        // Rule 11's click half, and the same one arm the hover path takes. The three it
        // replaces each spelled out the two activation shapes by hand — a control FACE
        // dips and commits on the spring-back, a table row commits on the button-down —
        // which is exactly what `ElemKind` says once, per element, for every owned screen.
        if super::bridge::owns_input(&app.pages) {
            let (cx, cy) = ptr_xy(&app.ev);
            app.inputs.push(super::bridge::click_input(
                cx,
                cy,
                plx_machine::machine::Tick { ms: app.last_input, dt_us: 0 },
            ));
            return;
        }
    } else if et == SDL_MOUSEBUTTONUP {
        app.last_input = clock::now();
        // a click that armed the tvOS press (a detail card) releases on the button-up,
        // the pointer's twin of the OK key-up: without it the dip would sit there until
        // press.rs's dropped-key-up ceiling fired. A no-op when no press is in flight.
        app.input.press.release(app.last_input);
        // …and the same release for the TREE's press machine, which has no pointer-up of
        // its own: `dispatch`'s ingest reaches `InputMachine::release` from exactly one
        // place, an `Ok` key on the `Up` edge, so that is what a button-up becomes here
        // (`bridge::release_input` carries the reasoning and why it is inert everywhere
        // else). Unconditional on ownership for `press.release`'s reason above — a
        // release with nothing armed is a no-op, and asking `owns_input` would drop the
        // release of a press armed on the frame a surface began to close.
        app.inputs.push(super::bridge::release_input(plx_machine::machine::Tick {
            ms: app.last_input,
            dt_us: 0,
        }));
        // …and the button is up, which is all this arm still says. Committing the player's drag
        // from here was the other half of the split ownership phase 12 retires: the release above
        // reaches `PlayerScreen::handle_release` as the `Ok`/`Edge::Up` the tree already gets, and
        // the page commits its own preview with `PlayerReq::CommitSeek`.
        app.ptr.button_down = false;
    } else if et == SDL_MOUSEWHEEL {
        app.last_input = clock::now();
        if app.last_input.wrapping_sub(app.ptr.last_wheel) > 250 {
            app.ptr.last_wheel = app.last_input;
            // **The host reads a DIFFERENT offset, and this one is not the LG-fork
            // shift `decode_key` documents — it is macOS `libSDL2` again being
            // sdl2-compat forwarding into SDL3.** `SDL_MouseWheelEvent` on real SDL2
            // (what the television runs) carries a plain `Sint32 y` at +20, which is
            // what production code always read. Measured 2026-09-02 by dumping the
            // polled bytes of an injected -40 tick on this host: `+20` came back 0, and
            // -40.0 arrived instead as the FLOAT `preciseY` field at `+32` — SDL2's own
            // struct gained that field in 2.0.18 for fractional trackpad scroll, and
            // sdl2-compat's round trip apparently only forwards it, leaving the legacy
            // integer field zeroed for both a genuine trackpad tick and anything
            // `SDL_PushEvent`d in this shape (item 13's `wheel:<dy>` FIFO token hit
            // this the first time anything synthesized a wheel event at all — nothing
            // needed one before). `cfg!`, not `#[cfg]`, so both arms keep compiling.
            let dy = if true {
                rd_f32(&app.ev, 32).round() as i32
            } else {
                rd_i32(&app.ev, 20)
            };
            // the wheel scrolls VERTICALLY only, and only on routes with a vertical
            // flow (it used to drive home's focus behind every other screen)
            //
            // Item 13: an owned surface takes the wheel BEFORE the route dispatch below
            // ever sees it — otherwise a wheel tick over Settings/Privacy/Legal fell
            // through to whatever route sat behind it (Home's own hero/grid dive, a
            // Detail scroll, …), which is the same ownership question the key ladder
            // answers for a fresh press, asked here for the wheel instead. It is the
            // same `owns_input` the other three arms ask, so the four cannot drift.
            if dy == 0 {
                // a fractional trackpad tick that rounded to nothing (hostsim), or a
                // `wheel:0` token — not a step in either direction
                return;
            }
            if super::bridge::owns_input(&app.pages) {
                // A tick becomes the DIRECTION KEY it stands for (`bridge::wheel_input`),
                // rather than a `Wheel` the family's tables would each have to interpret:
                // one tick is one row, which is what `on_updown(±1)` meant. `RepeatGate`
                // is deliberately NOT applied — a wheel gesture's ticks are the user's own
                // cadence and the gate is armed against the remote's 50 ms hardware
                // repeat, so sharing it would let a held key mute a scroll and vice versa.
                app.inputs.extend(super::bridge::wheel_input(
                    dy,
                    plx_machine::machine::Tick { ms: app.last_input, dt_us: 0 },
                ));
            }
        }
    }
}

/// A foreground Load launched — straight from the DID edge, or from the frame that landed a flight
/// the restore was waiting on (`poll_foreground_flight`): mount the player and pin its transport.
/// Permission to advance playback is independent of the retained page stack.
fn playback_may_run(app: &App) -> bool {
    app.window_activity.allow_present(true)
}

/// The playback side of an iteration before the tick: the engine pump, EOS, the HUD's rest
/// position and the paused seek — everything that reads `fr.now` and no `dt`.
pub(crate) unsafe fn playback_tick(app: &mut App, fr: &mut Frame) {
        if playback_may_run(app) {
            if is_started() {
                plx_media::player::pump(&mut app.player.session, &mut app.adapters.player, fr.now);
            }
        }
        // **The ONE place the Player machine is asked whether the hardware video plane is bound**
        // (spec §9), immediately after the pump that advances the ACB bind transaction and BEFORE
        // the container tree's frame, so the whole iteration reads one answer. Outside the
        // `is_started` guard on purpose: a stop retires the engine and `started` with it, and the
        // FALSE edge is exactly the one nobody would otherwise carry.
        //
        // Everything downstream READS `app.player.video_plane_bound`; nothing recomputes it, and
        // nothing keys the plane's consequences on the route any more.
        if let Some(bound) = plx_media::player::observe_video_plane(&mut app.player, &mut app.adapters.player) {
            // One edge, three gates. `set_video_plane_bound` has already told the LIVE one
            // (`plx_machine::idle`); these are the two §4.4 machines — the one `App` owns and the
            // dispatcher's. `PresentEvent::VideoPlane` has no other source.
            app.present.note(plx_machine::present::PresentEvent::VideoPlane(bound));
            app.pages.present.note(plx_machine::present::PresentEvent::VideoPlane(bound));
        }
        // end-of-stream: the pipeline drained at the credits → leave the player (back to the
        // folder it was launched from) instead of freezing on the last frame.
        if playback_may_run(app) && super::bridge::player(&app.pages).is_some() && plx_media::player::ended() {
            // The file's record is settled as finished.
            super::local_watch::note_ended();
            exit_player(&mut app.player.session, &mut app.adapters.player, &mut app.pages);
        } else if playback_may_run(app)
            && plx_media::player::ended()
            && app.adapters.player.is_live()
            && super::bridge::player(&app.pages).is_none()
        {
            // The same end with no player page to leave: nothing else would ever stop this
            // engine.
            log("EOS: stopping an off-route engine nothing owns");
            plx_media::player::stop_bufferfeed(&mut app.player.session, &mut app.adapters.player);
        }
        // (The lost-keyup safety net and the CLIENT-SIDE LONG-PRESS REPEAT stood here — the one
        // hold-to-move path for every discrete focus list, driven by `App::held_key` at 110 ms so
        // the feel was identical everywhere and independent of the remote's hardware repeat delay.
        // Both are gone with phase 10, and it is the same removal twice over: the net only existed
        // to stop that timer running forever on a dropped key-up.
        //
        // Every list it ever served is an owned screen or a surface on the dispatcher now. The
        // Settings family left in 5b, Home and the Library in 8, Search in 7, the player's four
        // panels in 9 — each applying the SAME 110 ms cadence to the hardware's ~50 ms
        // `Edge::Repeat` itself (`screens::registry::PANEL_REPEAT_MS` / `RepeatGate`) — and
        // the item context menu, the last consumer, in 10. With no arm left to fire, `HeldKey::arm`
        // had no caller and `App::held_key`'s `sym`/`since`/`last_rep`/`alive` had no producer;
        // `down_sym`, which is a fact about the PHYSICAL key rather than about this timer, survives
        // as `App::down_sym`.)
        // (The HUD used to be held up from here while a panel was open. The panel that IS open
        // says so itself now, from its own `Tick` — `PlayerOverlayScreen::step` pushes
        // `PlayerReq::ExtendHud`.)
        // (The scrub gesture's per-frame block stood here: the continuous accelerating advance,
        // the `SCRUB_LOST_MS` lost-keyup safety commit and the tap-release debounce, all three run
        // against the mounted `PlayerScreen`'s own `scrub` from the loop. They are that screen's
        // `Tick` now — `step_scrub_hold` and `step_tap_commit`, formula for formula — which is
        // what makes the field have one owner. Phase 12, PX-PLAYER.)
        if matches!(app.route(), AppArg::Player) {
            let paused_now = paused();
            if let Some(player) = super::bridge::player_mut(&mut app.pages) {
                // when the HUD auto-hides, park focus back on the scrubber so the next reveal
                // is clean
                if !player.hud.visible(&mut app.player.session, fr.now, paused_now) {
                    player.hud.nav = HudNav::HOME;
                }
                player.publish();
            }
        }
        // hide the idle pointer during playback
        if matches!(app.route(), AppArg::Player)
            && !app.ptr.cur_hidden
            && !app.ptr.button_down
            && app.ptr.last_motion != 0
            && fr.now.wrapping_sub(app.ptr.last_motion) > 3000
        {
            app.ptr.cur_hidden = true;
        }
        // re-pause after a resume the INSTANT the seek's frame is on screen. `frames()` counts
        // real "frame presented" callbacks (reset on seek), so >= 1 means the target frame is
        // already composited — re-freezing then shows it with the shortest possible play-blip
        // (a paused scrub must briefly Play to decode the frame; buffer-feed has no preroll).
        if resume_pend()
            && matches!(app.route(), AppArg::Player)
            && plx_media::player::seek_preroll_active()
            && seek_pending() < 0
            && frames() >= 1
            && playpos() + 15 * 1_000_000_000 >= app.repause_at
        {
            plx_media::player::finish_paused_seek(&mut app.adapters.player);
        }
}

/// Results that land BEFORE nav commit: the deferred OK press of the player's control row.
pub(crate) unsafe fn land_results(app: &mut App, fr: &mut Frame) {
    if !app.ok_armed {
        return;
    }
    // A press-and-hold opens nothing from here (an owned page's hold is its own press machine's);
    // it springs back.
    if app.input.press.is_long(fr.now) {
        app.ok_armed = false;
        app.input.press.cancel();
    } else if app.input.press.take_commit(fr.now) {
        app.ok_armed = false;
        // The deferred activation: the player's control row is the one press the loop arms.
        match app.route() {
            AppArg::Player => activate_player_row(fr.now,
                &mut app.pages,
                &mut app.bridge,
            ),
            _ => {}
        }
    } else if !app.input.press.is_active() {
        app.ok_armed = false; // long-press / cancelled — disarm without activating
    }
}

/// The update phase: the toast — `tick_drain` on the FRAMEDROP line.
pub(crate) fn update(fr: &Frame) {
    super::toast::update(fr.now);
    #[cfg(feature = "threadcheck")]
    plx_ui::runtime_warning::update(false);
}

/// The draw phase, entered only on a presenting frame: `clear_opaque_region` at entry, the
/// viewport, glass owners' prepare, the page pass, the surfaces bottom-to-top and the
/// instruments. Returns the viewport for the host-side screenshot that follows the draw.
pub(crate) unsafe fn draw(app: &mut App, fr: &mut Frame) -> (i32, i32, i32, i32) {
    #[cfg(feature = "threadcheck")]
    let _draw_scope = plx_base::task::watchdog::draw_scope();
            // The authored canvas, scaled UNIFORMLY into the drawable and centred. The shaders
            // divide every coordinate by `u_screen` (which stays 1920x1080), so this one call
            // is the entire logical->physical mapping — nothing else in the renderer knows the
            // drawable size. At 1:1 on every television seen so far; on any 16:9 surface a
            // plain scale with zero letterbox (1080p->4K is exactly 2x); and on an unexpected
            // aspect, letterboxed rather than stretched or stuffed into a corner. See `surface`.
            let (vx, vy, vw, vh) = plx_base::surface::viewport();
            glViewport(vx, vy, vw, vh);
            // EVERY screen draws inside ONE panic barrier. `plex_run` is `extern "C"` (main.c calls
            // it), so a panic unwinding out of a screen's draw is UB the toolchain turns into
            // abort() — the app dies and a live Starfish session is torn down mid-Feed(), on a
            // device with no debugger. Guarding HERE, at the route→screen dispatch, is what makes
            // that structural: a screen added later is covered without its author remembering,
            // which is exactly how every module but home.rs ended up unguarded. The barrier wraps
            // the WHOLE dispatch rather than each `::draw()` so draw ORDER and z-stacking are
            // untouched — a panic in the HUD abandons the rest of the frame instead of stacking the
            // overlays onto a half-built one — and so `ui::guard`'s scissor repair runs once, after
            // the last screen that could have left a clip armed. See `ui::guard` for what this does
            // NOT cover (worker-thread panics, aborts, half-mutated state). Everything inside is a
            // read of loop state, so the closure only borrows; nothing is moved out of the loop.
            {
                plx_ui::guard(|| {
                    if fr.player {
                        glClearColor(0.0, 0.0, 0.0, 0.0);
                        // The frame's first framebuffer-0 command, where this driver parks the
                        // wait for a free back buffer: spanned like every other route's `clear`.
                        plx_base::diag::spans::span("clear", || glClear(GL_COLOR_BUFFER_BIT));
                        // ONE resolve of which surface owns the "pipeline is working" signal,
                        // handed to both the transport and the read-out, so the centred read-out
                        // and the transport's inline spinner can never both light in the same
                        // frame. Resolved HERE (not beside `ctrl` at the top of the iteration)
                        // because `player::pump` republishes the state mid-iteration and this must
                        // be the post-pump value.
                        //
                        // `transport` is the other per-frame fact the instance cannot resolve for
                        // itself: the transport's MIDDLE is hidden behind an open Chapters strip,
                        // and which panel is up is the container's answer.
                        let busy = plx_appkit::player_hud::busy(&app.player.session);
                        let open_kind = super::bridge::player_overlay_kind(&app.pages);
                        let bare_middle = !matches!(open_kind, Some(plx_screens::player::overlay::OverlayKind::Chapters));
                        // The top-menu item whose panel is up keeps its selected plate.
                        let menu_open = open_kind.and_then(|kind| {
                            use plx_appkit::player_hud::MenuItem;
                            use plx_screens::player::overlay::OverlayKind;
                            match kind {
                                OverlayKind::Info => Some(MenuItem::Info),
                                OverlayKind::Chapters => Some(MenuItem::Chapters),
                                OverlayKind::Tracks { tab: 1 } => Some(MenuItem::Subtitles),
                                OverlayKind::Tracks { .. } => Some(MenuItem::Audio),
                                OverlayKind::Timing => None,
                            }
                        });
                        // Both subtitle paths lift clear of the transport for the same reason and
                        // by the same test — an open track menu counts, since that is exactly when
                        // the user is reading the bottom of the screen. The Timing capsule is the
                        // one panel that instead HIDES the transport outright (`HudPolicy::Hidden`
                        // outranks `Lifted` — see `bridge::player_hud_policy`).
                        let hud_policy = super::bridge::player_hud_policy(&app.pages);
                        if let Some(player) = super::bridge::player_mut(&mut app.pages) {
                            player.busy = busy;
                            player.transport = bare_middle;
                            player.menu_open = menu_open;
                            player.set_hud_policy(hud_policy);
                        }
                        // THE PAGE, and its panels: `Dispatcher::draw(.., true)` runs the page
                        // pass (subtitles, the transport, the read-out — `PlayerScreen::draw`) and
                        // then the surfaces bottom-to-top, which is exactly the order the four
                        // hand-written `*_panel::draw()` calls used to state by hand.
                        //
                        // `popover::host::begin_frame` is deliberately NOT called on this path and
                        // never was: what is behind these panels is a hardware video plane GL
                        // cannot read back, so there is no host snapshot to take
                        // (`RenderStrategy::VideoPlane`).
                        app.pages.draw(&mut app.bridge, true);
                    } else {
                        // Open the shared popover frame FIRST: it takes this frame's page damage
                        // and decides whether the frozen-host snapshot still describes the page. Route-agnostic by construction —
                        // see `ui::popover::host::begin_frame`.
                        plx_base::diag::spans::span("host", || plx_ui::popover::host::begin_frame(fr.underlay_moving));
                        use plx_ui::frame::backdrop::Z;
                        let layers = app.pages.backdrop_layers(plx_ui::nav::page_alpha());
                        app.glass.sources.borrow_mut().begin(layers);
                        let sources = app.glass.sources.clone();
                        let _visible_walk = app.glass.walk(Z::ALL);
                        let page_owned = super::bridge::page_owned(&app.pages);
                        let host_replaced = super::bridge::host_replaced(&app.pages);
                        let plan = super::bridge::page_plan(host_replaced, page_owned);
                        // One dispatcher walk supplies declarations, strict source prefixes and
                        // the visible frame. Only the visible walk advances the host cache.
                        let mut page = |ceiling: Z| {
                            let _host = (!plx_ui::frame::backdrop::discovering() &&
                                (ceiling == Z::ALL || plx_ui::popover::host::held_ceiling().is_some_and(|z| z < ceiling)))
                                .then(plx_ui::popover::host::page_pass);
                            if page_owned {
                                app.pages.draw_with_glass_below(&mut app.bridge, &mut app.glass, true, ceiling);
                            } else {
                                plx_gfx::gfx::frame_clear(plx_ui::theme::CLEAR_RGB.0, plx_ui::theme::CLEAR_RGB.1, plx_ui::theme::CLEAR_RGB.2);
                            }
                            if plan != super::bridge::PagePlan::SurfacesOnly && Z::OPENER < ceiling {
                                let _opener = plx_ui::frame::backdrop::layer(Z::OPENER, false);
                                app.bridge.redraw_opener(&app.pages);
                            }
                        };
                        {
                            let _declarations = plx_ui::frame::backdrop::discover(sources.clone());
                            plx_base::diag::spans::span("disc", || page(Z::ALL));
                        }
                        sources.borrow_mut().resolve();
                        // `pitv-framedrop` armed: which recorded draw invalidated a source.
                        // Empty (no allocation, no work) unarmed.
                        for line in sources.borrow_mut().take_diffs() {
                            log(&line);
                        }
                        let jobs = sources.borrow().jobs();
                        // Independent bands replay only the strict prefix below their ceiling.
                        // A band intersecting lower glass captures that composite in visible order.
                        for (ceiling, rect) in jobs {
                            let _source_walk = plx_ui::frame::backdrop::enter(sources.clone(), ceiling);
                            let reg = plx_gfx::gfx::blur_region_for(plx_ui::frame::backdrop::rises(ceiling), rect.x, rect.y, rect.w, rect.h);
                            // `src`: one blur source job, its replay of the page prefix included.
                            plx_base::diag::spans::span("src", || {
                                if plx_gfx::gfx::blur_snapshot_direct(reg, &mut || page(ceiling)) {
                                    plx_gfx::gfx::retain_backdrop(ceiling);
                                }
                            });
                        }
                        // An opaque route ground replaces the page entirely.
                        if plan != super::bridge::PagePlan::SurfacesOnly {
                            (page(Z::ALL));
                        }
                        // Owned pages already dispatched their surfaces in the visible walk.
                        if plan != super::bridge::PagePlan::Owned {
                            app.pages.draw_with_glass_below(&mut app.bridge, &mut app.glass, false, Z::ALL);
                        }
                        drop(_visible_walk);
                        // The on-screen counter, off the player route (chrome over video). It draws
                        // the last completed `fps=` window — frames actually swapped, not loop
                        // iterations. It necessarily HOLDS its last painted value on a settled
                        // screen until the ordinary keepalive buys another present; waking the
                        // renderer for the diagnostic would falsify the number it is showing.
                        //
                        // NOT in a release build (`make RELEASE=1` → --no-default-features). This
                        // costs the fps scenes nothing: they grade the once/sec heartbeat in the
                        // EVENT LOG, never the pixels, so `loop_floor`/`fps_floor`/`fps_ceiling`
                        // are unaffected by whether the digits are painted.
                        // A supersampled simulator render (`surface::render_scale`) exists to be
                        // captured, so it leaves the diagnostic digits out of the picture.
                        #[cfg(feature = "devtools")]
                        if plx_base::surface::render_scale() == 1 {
                            let fps_col = if app.buffer_flip_count < 30 {
                                plx_ui::theme::DIAG_FLIP_A
                            } else {
                                plx_ui::theme::DIAG_FLIP_B
                            };
                            plx_gfx::gfx::draw_number(
                                app.fps_shown,
                                SCR_W as f32 - 70.0,
                                64.0,
                                46.0,
                                fps_col.as_ptr(),
                            );
                        }
                    }
                    plx_ui::anim::draw_overlay(); // dev diagnostic overlay (all routes)
                    super::toast::draw();
                    #[cfg(feature = "threadcheck")]
                    plx_ui::runtime_warning::draw(false);
                });
            };
    (vx, vy, vw, vh)
}

/// The iteration's report: the measure fault, the cold-open lines and the FRAMEDROP line.
pub(crate) unsafe fn report(app: &mut App, fr: &mut Frame) {
    fr.rn = super::words::route_word(&app.route());
    let rn = fr.rn;
    if plx_gfx::text::take_measure_fault() && !app.measure_fault_logged {
        // once per process: the layout that was built on estimates is the thing to go and look at
        app.measure_fault_logged = true;
        log("text: a width was measured with NO FONT loaded — layout is on average-advance estimates");
    }
    if fr.rn != app.last_route_reported {
        app.last_route_reported = fr.rn;
    }
    // `pitv-focusx`: the card read-out belongs to the frame just sampled.
        // `coldopen screen=<name> ms=<n> prepared=<bool>` — one line per screen MOUNT, on every
        // build, no trigger (spec §8.4). The dispatcher owns both ends of the measurement (the
        // mount at nav commit, the first prepared+drawn frame); this drains what it closed.
        // Ungated by `fr.present` on purpose: a line closes on a frame that DID draw, so by the
        // time it is here the presenting is already decided and done.
        for line in app.pages.take_cold_open_lines() {
            log(&line);
        }
        // frame-drop detector: attribute slow frames to pump(uploads)/draw/swap(GPU).
        // ONE tail for every route — this used to live only on the non-player path, which left
        // /tmp/pitv-framedrop dead during playback (the timings were collected, then a
        // `continue` threw them away).
        // `present` gates this too: a frame the idle gate skipped drew nothing, so grading it
        // would drag `worstframe` toward zero and read as a perf WIN. A skipped frame is not a
        // fast frame — it is an absent one, and `fps=` on the heartbeat is where it shows up.
        if fr.present {
            // **The four upload/card counters are drained HERE, on every presented frame** — not
            // inside the closure below, which the instrument calls only after the threshold
            // check. That is what they used to be: a slow frame's `up=`/`px=`/`cards=`/`off=`
            // covered every frame since the PREVIOUS slow one, and read as this frame's cost.
            // Two atomic swaps a frame is what "per frame" costs; the comment here claimed it
            // was already being paid.
            let (uploads, upload_px) = super::adapters::poster::take_upload_stats();
            let (cards, cards_off) = plx_gfx::gfx::take_card_stats();
            app.instr.note_frame_counters(plx_base::diag::heartbeat::FrameCounters {
                uploads,
                upload_px,
                cards,
                cards_off,
            });
            // Taken on every presented frame for the counters' reason above: a span belongs to
            // the frame it ran in, never to the next slow one.
            let spans = plx_base::diag::spans::take();
            for line in app.instr.frame_drop_lines(&|| {
                format!(
                    "route={rn} dip={} {spans}",
                    app.pages.dip_word(),
                )
            }) {
                log(&line);
            }
        } else {
            // Spans recorded on a frame that did not present (`scen`/`pump`/`land`, `feed`, the
            // engine's `sfv`/`sfa`) belong to no FRAMEDROP line: left in place they would print on
            // the next presented frame's.
            let _ = plx_base::diag::spans::take();
        }
}

/// The once/sec heartbeat line and its counters.
pub(crate) unsafe fn heartbeat(app: &mut App, fr: &mut Frame) {
    let rn = fr.rn;
        if loop_tick(&mut app.iters_ct, &mut app.loop_t, &mut app.loop_shown, fr.now) {
            // once/sec render heartbeat — greppable without reading the on-screen counter.
            // The harness parses `loop=(\d+) route=(\w+)(?: overlay=(\w+))?` (tests/run.py), so
            // the player's overlay tag stays right after route= and worstframe= stays LAST.
            //
            // RENAMED 2026-08-01, and the old name was REUSED, so a log predating this reads
            // as the opposite of what it says: the field that used to be `FPS=` is now `loop=`,
            // and `fps=` now means what it always should have — frames actually presented,
            // previously `pres=`. An old `FPS=60` is a LOOP rate and says nothing about frames.
            let ov = super::words::overlay_suffix(&app.pages, &app.route());
            // `pos=<s>` rides the heartbeat while frames are actually being presented: the
            // same SHARED.playpos_ns the /:/timeline reporter posts, but at 1 Hz instead of
            // that reporter's 10s cadence. tests/run.py grades playback progress from this.
            // The cadence is the point: to OBSERVE a 15s climb through 10s samples you must
            // play ~30s, so the sparse signal was charging every case double its real floor.
            // Gated on is_playing() (not is_started()) — see that fn for the resume trap.
            let pos_ns = playpos(); // one read — the test and the value must agree
                                    // `play=<pm>` — MEDIA time advanced per WALL millisecond since the previous
                                    // heartbeat, in per mille. 1000 is the film running at speed; 670 is it crawling.
                                    //
                                    // **It is the only field on this line that can see a slow film**, and the reason
                                    // is worth carrying. Every buffer signal the adaptive controller reads is a
                                    // RESERVE, and a reserve is media time measured against this same playhead — so
                                    // when the playhead slows, the reserve stops draining, `slope` goes quiet and
                                    // every drain-derived trigger falls silent at exactly the moment the picture is
                                    // worst. `fps=` cannot see it either: that counts OUR GL swaps, and it sits at 60
                                    // through a stream the television is decoding at two thirds speed. `vtick`/`vgap`
                                    // come closest — they are the pipeline's own 5 Hz callback and they do respond to
                                    // gross starvation — but they are a cadence, not a rate, and the healthy reading
                                    // is 5/201 whatever the media clock is doing.
                                    //
                                    // NO magnitude gate, deliberately. A seek reads as a huge or negative value and a
                                    // catch-up leg as something above 1000; both are real observations and both are
                                    // things a reader wants to see. Inventing a "that must be a seek" threshold would
                                    // be a constant nobody can derive, and the analysis side (`tests/run.py`'s
                                    // `playback_rate`) already splits legs on the discontinuity itself.
            let playing = plx_media::player::is_playing(&app.player.session) && pos_ns > 0;
            let mut pos = String::new();
            if playing {
                pos = format!(" pos={}s", pos_ns / 1_000_000_000);
                if let Some((prev_ns, prev_ticks)) = app.play_prev {
                    let wall_ms = i64::from(fr.now.wrapping_sub(prev_ticks));
                    if wall_ms > 0 {
                        let media_ms = (pos_ns - prev_ns) / 1_000_000;
                        pos.push_str(&format!(" play={}pm", media_ms * 1000 / wall_ms));
                    }
                }
            }
            app.play_prev = if playing { Some((pos_ns, fr.now)) } else { None };
            // `vtick=<n> vgap=<n>ms` — the media pipeline's own `FRAMEREADY` cadence, the only
            // field on this line that comes from the VIDEO plane's side of the house. Every
            // other number here describes the graphics plane: `fps=` counts our GL swaps and
            // `worstframe=` times our own draw, and both sit at a healthy 60 / sub-millisecond
            // through playback that visibly stutters, because the decoded picture is
            // composited by the television on a surface we never touch.
            //
            // **It is not a frame rate** — `player::vplane_take` says why, and the healthy
            // reading is 5 / 201 on every stream. Read it as liveness: `vtick=0` is a pipeline
            // that has stopped, and a steady 5 / 201 under a stutter report is the pipeline
            // saying the fault is not in anything this process can reach. Drained here, so it
            // must be taken every heartbeat while playing or the worst gap accumulates.
            let vp = if plx_media::player::is_playing(&app.player.session) {
                let (vtick, vgap) = plx_media::player::vplane_take();
                format!(" vtick={vtick} vgap={vgap}ms")
            } else {
                String::new()
            };
            // `fps=<n>` — frames actually SWAPPED this second, which is what `plx_machine::idle` moves,
            // and the only field here that is a frame rate. `loop=` counts LOOP iterations: it
            // is the app's liveness signal and `pos=` is anchored to it, so it must not read 0
            // on a screen that is merely idle. The pair is the diagnostic — `loop=62 fps=0` is
            // a settled screen doing its job, `loop=0` is an app in trouble, and `fps=0` on its
            // own is not a fault at all. The dev on-screen counter draws this same drained
            // value, cached below; it never reads a second presentation counter.
            let pres = plx_machine::idle::take_presents();
            #[cfg(feature = "devtools")]
            {
                app.fps_shown = pres.min(i32::MAX as u32) as i32;
            }
            // `worstframe=` stays LAST of the graded fields (both harness regexes anchor on it);
            // `worstprep=` follows it, ungated by present. Both are empty unarmed. After them
            // ride the frame plan's four (spec §8.4), which print in every build because none of
            // them costs a measurement — every one is a counter its owner already keeps:
            //
            // * `carried=`/`dropped=` — the dispatcher's queue depth carried into the next frame,
            //   and the deliveries dropped since the last heartbeat. `carried` had only ever
            //   surfaced as the GROWTH-streak warning (`dispatch: carried=N growing`), which fires
            //   at a slope and says nothing about the level; `dropped` had never surfaced at all.
            // * `budget=<admitted>/<refused>[/solo:<class>]` — the frame budget's second, drained
            //   here and nowhere else (`take_frame_stats` accumulates until its reader takes it,
            //   which is why this is the once-a-second call and not a per-frame one).
            // * `evicted_hot=` — glyphs evicted inside their hot window (`text.rs`, phase 11).
            let budget = app.pages.budget.take_frame_stats();
            let (carried, dropped) = app.pages.take_heartbeat_counters();
            let tail = app.instr.heartbeat_tail(
                plx_base::diag::heartbeat::HeartbeatFields {
                    carried,
                    dropped,
                    admitted: budget.admitted,
                    refused: budget.refused,
                    solo: budget.solo.map(|c| c.name()),
                    evicted_hot: plx_gfx::text::take_evicted_hot(),
                },
                None,
            );
            log(&format!(
                "loop={} route={rn}{ov}{pos}{vp} fps={pres}{tail}{SIM_TAG}",
                app.loop_shown
            ));
        }
}

/// Teardown after the loop: stop the feed, close the poster workers, quit SDL.
pub(crate) unsafe fn shutdown(
    ps: &mut plx_media::playback::PlaybackSession,
    pa: &mut plx_media::player::adapter::PlayerAdapter,
) {
    if is_started() {
        plx_media::player::stop_bufferfeed(ps, pa);
    }
    crate::app::adapters::poster::shutdown();
    SDL_Quit();
}
