//! Boot: the desktop window size, the panic logger, the loop-rate tick, and [`boot`] — SDL, the
//! window, GL, text, the poster workers and the `App` literal.

use super::*;

/// The window size a DESKTOP should open at, in points: the authored 1920x1080 canvas divided by
/// the smallest whole number that fits the usable display area.
///
/// **An exact divisor, never a best fit.** `surface::scale` will letterbox any drawable it is
/// given, so an arbitrary size would *work* — it would just be soft, because every glyph and icon
/// mask in this app is rasterized for a 1:1 surface (`gfx::snap`, and the crispness contract in
/// `theme.rs`) and a fractional scale resamples all of it. 1/1, 1/2 and 1/3 keep whole texels whole.
///
/// The television is untouched by any of this: it takes the panel, and the canvas IS the surface.
/// A Mac is the case the `surface` doc was written against — 1920x1080 exceeds the usable area of
/// every laptop display Apple ships, so asking for it flatly would put the title bar above the
/// screen and the bottom of the interface under the Dock.
///
/// Falls back to the canvas size if SDL cannot answer, which is the behaviour this replaced.
pub(crate) fn desktop_window_size() -> (c_int, c_int) {
    // `PITV_WIN=<w>x<h>` overrides the fit entirely — `make sim-shot SIM_W=1920 SIM_H=1080`.
    // It exists because the fit below is chosen for a HUMAN looking at a window, and a screenshot
    // is not that: on a 1x display the divisor lands on 2 and every shot comes back 960x540, which
    // is half the canvas the UI is authored at. A hairline, a 1px edge-sheen and a snapped glyph
    // are exactly the things that do not survive that, so a shot taken to JUDGE the interface has
    // to be asked for at full size. Off-screen edges are fine for a headless grab: the drawable is
    // the window's own framebuffer, not the part of it the compositor happens to show.
    if let Some(v) = std::env::var_os("PITV_WIN") {
        let v = v.to_string_lossy().to_lowercase();
        if let Some((w, h)) = v.split_once('x') {
            if let (Ok(w), Ok(h)) = (w.trim().parse::<c_int>(), h.trim().parse::<c_int>()) {
                if w > 0 && h > 0 {
                    return (w, h);
                }
            }
        }
    }
    let mut r = [0 as c_int; 4]; // SDL_Rect: x, y, w, h
    let ok = unsafe { SDL_GetDisplayUsableBounds(0, r.as_mut_ptr()) } == 0;
    let (uw, uh) = (r[2], r[3]);
    if !ok || uw <= 0 || uh <= 0 {
        return (SCR_W, SCR_H);
    }
    // A little headroom under the usable bounds: a window flush against them reads as a fullscreen
    // that went wrong rather than as a deliberate size.
    for div in 1..=3 {
        let (w, h) = (SCR_W / div, SCR_H / div);
        if w <= (uw as f32 * 0.95) as c_int && h <= (uh as f32 * 0.95) as c_int {
            return (w, h);
        }
    }
    (SCR_W / 3, SCR_H / 3)
}

/// Log every Rust panic (message + source location + thread) to the event log BEFORE it unwinds.
/// A panic that crosses an extern "C" boundary (libav calling `ff::read_cb`/`seek_cb`) aborts the
/// process, and by then the message is gone, so capturing it here is the only way to see WHAT
/// panicked.
pub(crate) fn install_panic_logger() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "?".into());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic>".into());
        let cur = std::thread::current();
        let thread = cur.name().unwrap_or("?");
        log(&format!("*** RUST PANIC [{thread}] at {loc}: {msg}"));
        default(info);
    }));
}

/// Advance the once-per-second LOOP-RATE window: bump `iters_ct` and, when a full second has
/// elapsed, recompute `loop_shown`, reset the window, and return `true` so the caller logs the
/// heartbeat with its own route/overlay tag. Shared by the player and home/detail draw paths.
///
/// This counts **loop iterations, not frames**. Since the present gate (`plx_machine::idle`) landed the two
/// are different numbers, and conflating them is the single most reliable way to misread this app:
/// a settled screen runs the loop at the `IDLE_POLL_MS` rate while swapping nothing. The frame
/// count lives beside it in the heartbeat as `fps=`, from `plx_machine::idle::take_presents`.
pub(crate) fn loop_tick(iters_ct: &mut i32, loop_t: &mut u32, loop_shown: &mut i32, now: u32) -> bool {
    *iters_ct += 1;
    if now.wrapping_sub(*loop_t) < 1000 {
        return false;
    }
    *loop_shown = (*iters_ct as f32 * 1000.0 / now.wrapping_sub(*loop_t) as f32 + 0.5) as i32;
    *iters_ct = 0;
    *loop_t = now;
    true
}

/// Everything before the loop: SDL and the window, GL, text, the poster workers and the `App`
/// literal. An early exit is the process exit code.
///
/// `mt`, THE main-thread token, MOVES into `App.adapters.player` here, from where a
/// `&mut PlayerAdapter` is the proof it is still held.
pub(crate) unsafe fn boot(
    mt: plx_base::task::MainThread,
) -> Result<App, c_int> {
    SDL_SetMainReady();
    SDL_SetHint(c"SDL_VIDEO_ALLOW_SCREENSAVER".as_ptr(), c"0".as_ptr());
    if SDL_Init(SDL_INIT_VIDEO) != 0 {
        log("SDL_Init failed");
        return Err(1);
    }
    {
        let d = SDL_GetCurrentVideoDriver();
        if !d.is_null() {
            log(&format!(
                "video driver: {}",
                std::ffi::CStr::from_ptr(d).to_string_lossy()
            ));
        }
    }
    // Desktop GL 4.1 core: everything this renderer does (a real VBO, RGBA/UNSIGNED_BYTE
    // textures) is core-profile-legal, and the shader sources are adapted at compile time by
    // `gfx::glsl_preamble`, which reads the driver's GLSL version rather than assuming.
    SDL_GL_SetAttribute(A_CTX_PROFILE_MASK, CTX_PROFILE_CORE);
    SDL_GL_SetAttribute(A_CTX_MAJOR, 4);
    SDL_GL_SetAttribute(A_CTX_MINOR, 1);
    // full 32-bit RGBA so the video plane shows through
    SDL_GL_SetAttribute(A_RED, 8);
    SDL_GL_SetAttribute(A_GREEN, 8);
    SDL_GL_SetAttribute(A_BLUE, 8);
    SDL_GL_SetAttribute(A_ALPHA, 8);
    SDL_GL_SetAttribute(A_BUFFER_SIZE, 32);
    // ...and NO depth or stencil: every screen is painter's-algorithm 2-D, drawn back to front,
    // and the one scissor user (`gfx::clip_set`) is a scissor, not a stencil.
    SDL_GL_SetAttribute(A_DEPTH, 0);
    SDL_GL_SetAttribute(A_STENCIL, 0);
    let (wx, wy, ww_req, wh_req) = {
        let (w, h) = desktop_window_size();
        (0x2FFF_0000u32 as c_int, 0x2FFF_0000u32 as c_int, w, h)
    };
    let title = c"PiTV";
    let window_flags = SDL_WINDOW_FLAGS;
    // The Linux build is a media player on a television: full screen on the display it opens on
    // (`SDL_WINDOW_FULLSCREEN_DESKTOP`, no mode switch), unless `PITV_FULLSCREEN=0` asks for
    // the desktop window. A fixed `PITV_WIN` size (screenshots) keeps the window too.
    let fullscreen = !(std::env::var_os("PITV_FULLSCREEN").is_some_and(|v| v == "0")
        || std::env::var_os("PITV_WIN").is_some());
    let window_flags = if fullscreen { window_flags | 0x1001 } else { window_flags };
    // Created AT the display's size, not resized to it afterwards: the GL surface of a window that
    // goes full screen after creation can stay at the requested size for the first frames (seen
    // under X11 without a window manager), which draws the canvas into one corner.
    let (wx, wy, ww_req, wh_req) = if fullscreen {
        let mut r = [0 as c_int; 4];
        if SDL_GetDisplayBounds(0, r.as_mut_ptr()) == 0 && r[2] > 0 && r[3] > 0 {
            (r[0], r[1], r[2], r[3])
        } else {
            (wx, wy, ww_req, wh_req)
        }
    } else {
        (wx, wy, ww_req, wh_req)
    };
    let win = SDL_CreateWindow(title.as_ptr(), wx, wy, ww_req, wh_req, window_flags);
    if win.is_null() {
        log("CreateWindow failed");
        return Err(1);
    }
    let ctx = SDL_GL_CreateContext(win);
    if ctx.is_null() {
        log("GL ctx failed");
        return Err(1);
    }
    plx_base::surface::probe(win);
    // vsync on → the frame rate locks to the panel refresh. WSLg's X11/GLX swap accepts interval 1
    // without blocking, so the software budget in `run` follows the same switch.
    let vsync_enabled = true;
    SDL_GL_SetSwapInterval(if vsync_enabled { 1 } else { 0 });
    let wslg_frame_pacing = vsync_enabled
        && std::env::var_os("WSL_DISTRO_NAME").is_some()
        && std::env::var("SDL_VIDEODRIVER").as_deref() == Ok("x11");
    {
        let r = glGetString(GL_RENDERER);
        let v = glGetString(GL_VERSION);
        let renderer = (!r.is_null()).then(|| std::ffi::CStr::from_ptr(r).to_string_lossy());
        if let (Some(renderer), false) = (&renderer, v.is_null()) {
            log(&format!("GL: {} / {}", renderer, std::ffi::CStr::from_ptr(v).to_string_lossy()));
        }
        // A CPU rasterizer's frame time is not a main-thread hang (`task::runtime_check`).
        #[cfg(feature = "threadcheck")]
        plx_base::task::runtime_check::note_renderer(renderer.as_deref());
    }
    // What EGL this context has — one boot-time read, logged. The probe asks EGL nothing unless
    // an EGL context is current on this thread, which is what makes it safe on GLX.
    plx_gfx::egl::probe();
    plx_gfx::gfx::init_gl();
    plx_gfx::text::init_text();
    plx_gfx::gfx::init_image();
    plx_gfx::gfx::init_blur();
    super::adapters::poster::init();
    // The player preferences (`settings.json`), before anything can draw a subtitle.
    plx_data::settings::load();
    #[cfg(not(test))]
    plx_platform::i18n::initialize();
    let bridge = super::bridge::Bridge::new(plx_base::diag::heartbeat::now_us);
    let pages = plx_ui::dispatch::Dispatcher::with_transition(Box::new(
        plx_ui::containers::transition::PageDip::new(),
    ));
    // Before the frame loop: the one blocking read of where each file was left.
    plx_data::local_progress::load();
    log("boot: local library");
    plx_media::player::acb_init(&mt);
    plx_media::ff::boot();
    let glass = plx_ui::frame::glass::GlassPlan::new();
    plx_media::player::seed_dev_track_names();
    // dev: /tmp/pitv-framedrop — the FRAME-DROP DETECTOR. Each frame is timed (pump / draw /
    // swap, no glFinish) and any frame whose total exceeds a threshold (ms; the file's content
    // overrides the 22 ms default) is logged with its phase breakdown.
    let framedrop = plx_base::devtrig::read("framedrop");
    let framedrop_on = framedrop.is_some();
    let framedrop_thresh: f64 = framedrop
        .and_then(|s| s.parse().ok())
        .filter(|v: &f64| *v > 0.0)
        .unwrap_or(22.0);
    let instr = plx_base::diag::heartbeat::Instruments::new(framedrop_on, framedrop_thresh);
    if framedrop_on {
        plx_base::diag::spans::arm();
    }
    let last_input = clock::now();
    let mut app = App {
        last_input,
        loop_t: last_input,
        iters_ct: 0,
        loop_shown: 0,
        #[cfg(feature = "devtools")]
        fps_shown: 0,
        play_prev: None,
        running: true,
        window_activity: super::window_activity::WindowActivity::new(),
        #[cfg(feature = "devtools")]
        buffer_flip_count: 0,
        down_sym: 0,
        modal_repeat: RepeatGate::IDLE,
        player: plx_media::player::machine::Player::new(),
        // The token stops being an argument here and becomes a field. `PlayerAdapter::new`
        // CONSUMES it, so the adapter is the only thing in the process that holds one.
        adapters: super::Adapters {
            player: plx_media::player::adapter::PlayerAdapter::new(mt),
        },
        repause_at: 0,
        ok_armed: false,
        last_route_reported: "",
        ptr: Pointer::IDLE,
        prev: 0,
        ev: [0u8; 128],
        // dev/testing remote: tokens written to the `pitv-remote` FIFO become synthetic key
        // events, consumed by the ONE real key handler.
        remote: crate::remote::Remote::open(),
        win,
        wslg_frame_pacing,
        t0: last_input,
        instr,
        measure_fault_logged: false,
        input: plx_ui::input::Input::new(),
        present: plx_machine::present::Present::new(),
        glass,
        // The application's page stack runs the route DIP.
        pages,
        inputs: Vec::new(),
        bridge,
    };
    // ROOT THE TREE AT THE LIBRARY. A `Root` on an empty stack mints the first entry, which is
    // what makes this a hard CUT — there is no outgoing screen to dip.
    super::bridge::nav_root(&mut app.pages, AppArg::Home);
    Ok(app)
}
