//! run_application — the Rust app core (was the body of src/main.c). Owns SDL init, the
//! event loop, input decode, the per-frame tick, draw orchestration, app lifecycle,
//! the buffer-feed pump orchestration, and the dev triggers. The C boot shim
//! (main.c) sets up the log and fallback crash tracer, calls the Rust image-marker and native-spool
//! entries when required, then calls `plex_run` (port.rs), which hands over to `run_application`.
//! The only application subsystem left in C is the starfish.c C++/ACB seam (the engine itself is
//! Rust: plx_media::player).
#![allow(non_upper_case_globals)]
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::sync::atomic::Ordering::Relaxed;

// ---- constants (SDL 2.0.4 + GLES2 + app) ----
const SDL_INIT_VIDEO: u32 = 0x20;
/// Appended to every heartbeat on a simulator build, and empty on a television.
///
/// The heartbeat is the app's perf surface: `tests/run.py --fps` grades `loop=` and `fps=` from it,
/// and the floors are calibrated to the SM9000's Mali. A Mac renders the same interface through a
/// completely different GPU, driver and compositor, so those numbers are not merely optimistic —
/// they are about a different machine. A log line is the unit that gets pasted into an issue or
/// handed between agents, so the disclaimer has to travel ON the line rather than sit in a doc.
const SIM_TAG: &str = if true {
    " sim=1"
} else {
    ""
};

/// OPENGL | FULLSCREEN on the television, which owns the whole panel.
///
/// The desktop asks for OPENGL | ALLOW_HIGHDPI — no fullscreen grab (hostile on a laptop) and
/// **not RESIZABLE**: `surface::probe` reads the drawable once, at boot, so a dragged edge would
/// leave the viewport describing a window that no longer exists, and the interface would sit in a
/// 1920x1080-shaped corner of the new one with every pointer hit landing somewhere else. The window
/// opens at an exact divisor of the canvas instead — see `desktop_window_size`.
///
/// ALLOW_HIGHDPI is what makes that divisor land on a **1:1 surface** on the Mac people actually
/// have: without it a Retina display gives a drawable equal to the window in POINTS, which the
/// compositor then doubles, so the whole interface is an upscale of a half-size render. With it,
/// the 960x540-point window `desktop_window_size` picks on a laptop has a 1920x1080 drawable —
/// `surface::scale() == 1.0`, the same 1:1 texel contract the television gets.
const SDL_WINDOW_FLAGS: u32 = if true {
    0x2 | 0x2000
} else {
    0x2 | 0x1
};
const GL_COLOR_BUFFER_BIT: c_uint = 0x0000_4000;
const GL_RENDERER: c_uint = 0x1F01;
const GL_VERSION: c_uint = 0x1F02;
// SDL_GLattr enum
const A_RED: c_int = 0;
const A_GREEN: c_int = 1;
const A_BLUE: c_int = 2;
const A_ALPHA: c_int = 3;
const A_BUFFER_SIZE: c_int = 4;
const A_DEPTH: c_int = 6;
const A_STENCIL: c_int = 7;
const A_CTX_MAJOR: c_int = 17;
const A_CTX_MINOR: c_int = 18;
const A_CTX_PROFILE_MASK: c_int = 21;
/// `SDL_GL_CONTEXT_PROFILE_CORE` — the simulator's only option on macOS. See the context request.
const CTX_PROFILE_CORE: c_int = 0x0001;
// event types
const SDL_QUIT: u32 = 0x100;
const SDL_KEYDOWN: u32 = 0x300;
const SDL_KEYUP: u32 = 0x301;
const SDL_MOUSEMOTION: u32 = 0x400;
const SDL_MOUSEBUTTONDOWN: u32 = 0x401;
const SDL_MOUSEBUTTONUP: u32 = 0x402;
const SDL_MOUSEWHEEL: u32 = 0x403;
/// The IME's in-progress COMPOSITION. Not acted on — the search field shows what has been
/// committed, so a preedit would put characters on screen the query does not contain — but LOGGED,
/// because the panel's word prediction is a replace and this is where its delete half would arrive
/// if it arrives at all. See the `"edit"` arm in the event ladder.
const SDL_TEXTEDITING: u32 = 0x302;
/// Text COMMITTED by the system keyboard — `crate::textinput`.
pub(crate) const SDL_TEXTINPUT: u32 = 0x303;
// keysyms, the OK/BACK predicates and `classify` — the key VOCABULARY the ladder below dispatches
// on — live in ui::consts (the single keycode home)
use plx_ui::consts::{
    classify, is_bound, is_ok, Key, SDLK_DOWN, SDLK_ESCAPE, SDLK_LEFT, SDLK_PAGEDOWN,
    SDLK_PAGEUP, SDLK_RETURN, SDLK_RIGHT, SDLK_UP, WCODE_CH_DOWN_KEY, WCODE_CH_UP_KEY, WCODE_PAUSE,
    WCODE_PLAY, WCODE_POINTER_HIDDEN, WCODE_STOP,
};
// The window we ASK SDL for. `surface::probe` then reads back what we actually got.
const SCR_W: c_int = plx_base::surface::LOGICAL_W as c_int;
const SCR_H: c_int = plx_base::surface::LOGICAL_H as c_int;

// Desktop-only window management. Apart for the mirror-image reason: a television owns the whole
// panel and never asks how big a display is, so on that build these would be dead code — which
// `[workspace.lints.rust] warnings = "deny"` makes a build failure, not a warning.
extern "C" {
    /// `SDL_GetDisplayUsableBounds` — the display minus the menu bar and the Dock, which is what
    /// a window may actually occupy. The out parameter is an `SDL_Rect`: exactly four `c_int`.
    fn SDL_GetDisplayUsableBounds(display: c_int, rect: *mut c_int) -> c_int;
}
extern "C" {
    /// `SDL_GetDisplayBounds` — the whole display, which the Linux build's full-screen window is
    /// created at (`boot`).
    fn SDL_GetDisplayBounds(display: c_int, rect: *mut c_int) -> c_int;
}

// Phase 1a of the UI restructure split this file: everything above `run_application` moved into
// the submodules below as a PURE move (`pub(crate)` widening only), glob-imported here so the
// loop body reads exactly as before. `run_application` itself is phase 1b.
pub(crate) mod adapters;
pub(crate) mod boot;
pub(crate) mod words;
pub(crate) mod clock;
mod window_activity;
pub(crate) mod events;
pub(crate) mod lifecycle;
pub(crate) mod playback;
pub(crate) mod local_launch; // `plxnative-sim <file>`: play a file from the command line
pub(crate) mod toast;
mod local_watch; // follows a local file's playback and keeps its place // the Linux build's toast, drawn by the app (the port delivers to it)
pub(crate) mod input;
pub(crate) mod bridge;
pub(crate) mod content;
pub(crate) mod run;
use self::boot::*;
use self::events::*;
use self::lifecycle::*;
use self::playback::*;
use self::input::*;
use self::content::*;

extern "C" {
    fn SDL_SetMainReady();
    fn SDL_SetHint(name: *const c_char, value: *const c_char) -> c_int;
    fn SDL_Init(flags: u32) -> c_int;
    fn SDL_GetCurrentVideoDriver() -> *const c_char;
    fn SDL_GL_SetAttribute(attr: c_int, value: c_int) -> c_int;
    fn SDL_CreateWindow(
        title: *const c_char,
        x: c_int,
        y: c_int,
        w: c_int,
        h: c_int,
        flags: u32,
    ) -> *mut c_void;
    fn SDL_GL_CreateContext(win: *mut c_void) -> *mut c_void;
    fn SDL_GL_SetSwapInterval(interval: c_int) -> c_int;
    fn SDL_Delay(ms: u32);
    fn SDL_PollEvent(event: *mut c_void) -> c_int;
    fn SDL_PushEvent(event: *const c_void) -> c_int;
    fn SDL_GL_SwapWindow(win: *mut c_void);
    fn SDL_Quit();
    fn glGetString(name: c_uint) -> *const c_char;
    fn glViewport(x: c_int, y: c_int, w: c_int, h: c_int);
    fn glClearColor(r: f32, g: f32, b: f32, a: f32);
    fn glClear(mask: c_uint);
}

use plx_base::eventlog::log;
// **The screen ARGUMENT is `screens::registry`'s** since restructure phase 10 (§2.1): the
// registry owns the concrete `ScreenArg` and the one `mount` match, so `app/` reads it here
// rather than declaring it. Imported at the tree's root because every module under `app/`
// that requests a navigation names it.
use plx_screens::registry::AppArg;


/// The adapter tree (spec §2.2). An adapter owns OS/FFI resources and holds no logical state; the
/// decisions live in the machines beside it.
pub(crate) struct Adapters {
    /// The Starfish/ACB session slot and the `MainThread` token — see [`plx_media::player::adapter`].
    pub(crate) player: plx_media::player::adapter::PlayerAdapter,
}

/// The app core's state, gathered from `run_application`'s loop-locals (UI restructure spec v4
/// §13, phase 1b-i: FIELDS ONLY — `run_application` keeps its shape and reads `app.<field>` where
/// it read a local). Phase 1b-ii extracts the coordinator's functions over `&mut App`; the machines of §2.2
/// replace these fields one phase at a time. Every field was a `let mut` before the `while
/// running` loop; the immutable boot-time values (dev flags, closures, the window) stay locals.
///
/// **`pub(crate)`, and every field with it (phase 10, `dev/scenarios.rs`'s prologue):** the dev-
/// trigger arms that used to live inline in `boot`/`run`/`content`/`mod` now live in
/// `crate::dev::scenarios`, a sibling of `app` rather than a descendant of it, so the fields and
/// helpers they still reach through `&mut App` need to be visible outside this module's subtree.
/// Nothing here becomes `pub` — only as wide as the crate.
pub(crate) struct App {
    last_input: u32,
    loop_t: u32,
    iters_ct: i32,
    loop_shown: i32,
    #[cfg(feature = "devtools")]
    fps_shown: i32,
    play_prev: Option<(i64, u32)>,
    running: bool,
    window_activity: window_activity::WindowActivity,
    #[cfg(feature = "devtools")]
    buffer_flip_count: u8,
    /// The sym we believe is PHYSICALLY DOWN right now — set by a fresh key-down, cleared by its
    /// key-up. It tells a real hardware auto-repeat from a PHANTOM one, which this television emits
    /// routinely: over the system keyboard the panel does not deliver a key-up for the press that
    /// raised it, so LG's key driver still believes OK is held and stamps the NEXT press with
    /// `state & 0x100`. Without this the repeat guard reads that as a repeat and drops it, so the
    /// first OK after every keyboard session does nothing — reported as "I have to click the search
    /// field twice for the keyboard to appear" (device-measured 2026-08-15). A repeat for a key we
    /// never saw pressed is not a repeat.
    ///
    /// **It is all that is left of `App::held_key`** (phase 10). The other five fields of `HeldKey`
    /// were the CLIENT-SIDE hold-to-move timer for every discrete focus list; the last list it
    /// still drove was the item context menu, and that is a `ModalStack` surface now which paces
    /// its own `Edge::Repeat` (`screens::registry::PANEL_REPEAT_MS`). This one is a fact about
    /// the physical key rather than about that timer, so it outlives it — as a bare `u32`, since a
    /// struct with one field is a name for a name. (`HeldKey` itself is gone with the timer:
    /// `PlayerScreen::held` had had no producer since phase 9 and wrote two constant zeros into
    /// the canonical state, which is exactly why it was safe to delete.)
    ///
    /// `scrubber: Scrub` and `hud: HudState` stood beside it through phase 8 and are gone: both are
    /// `PlayerScreen`'s own fields now (§9), so the loop borrows them out of the mounted instance
    /// and there is no second copy to keep in step.
    down_sym: u32,
    modal_repeat: RepeatGate,
    /// **The Player machine** (restructure spec §2.2, phase 9): the playback session that was
    /// `route::decision::SESSION`, the app-switch lifecycle that was `App.foreground`, and this
    /// frame's tick. Reached as a parameter from here down — `plx_media::player::machine`'s doc says
    /// why the pipeline's own handles are a separate field.
    pub(crate) player: plx_media::player::machine::Player,
    /// **The ADAPTERS** (restructure spec §2.2, phase 9) — the OS/FFI resources the machines act
    /// through. One so far: the Player's, which holds the native session that was
    /// `player::engine::ENGINE` together with the main-thread token that confines it.
    pub(crate) adapters: Adapters,
    repause_at: i64,
    pub(crate) ok_armed: bool,
    last_route_reported: &'static str,
    pub(crate) ptr: Pointer,
    // (`route`, `play_from`, `trail` and `nav_pending` stood here — four fields that between them
    // were a second navigation system: which page is on top, which page the live playback returns
    // to, the pages behind the one on screen, and the route change a fade is carrying. Each is the
    // CONTAINER's now — `NavStack`'s top entry, `screens::player::Origin`, the stack itself, and
    // `NavStack::pending` under a `PageDip`. D1, restructure phase 12.)
    pub(crate) prev: u32,
    ev: [u8; 128],
    remote: Option<crate::remote::Remote>,
    /// The SDL window (`SDL_CreateWindow`), for the swap.
    win: *mut c_void,
    /// WSLg accepts swap interval 1 on X11/GLX without pacing the compositor. This boot-scoped
    /// flag enables the host-only software frame budget while leaving native Linux and macOS on
    /// their driver swap behavior.
    wslg_frame_pacing: bool,
    /// Boot time (`SDL_GetTicks` at the end of boot): the origin of every dev-script delay AND
    /// the clock a replay restores from controlled initial inputs — a field BOTH sides
    /// write, so it stays here rather than on `dev::scenarios::Scenarios`.
    pub(crate) t0: u32,
    /// The frame's instruments: the eight phase stamps, FRAMEDROP, the per-second peaks
    /// (`diag::heartbeat`), armed by `plxnative-framedrop`.
    instr: plx_base::diag::heartbeat::Instruments,
    /// The Input machine: owner of the press (restructure spec §2.2); the ladders borrow it.
    pub(crate) input: plx_ui::input::Input,
    /// `text::take_measure_fault` has been reported once (the report is once per process).
    measure_fault_logged: bool,
    /// The present gate as a machine (spec §4.4). `plx_machine::idle` is still the product's verdict on
    /// this loop; this one receives the render cache's notes and is what `dispatch` takes over.
    present: plx_machine::present::Present,
    /// The frame plan's GLASS half (spec §8.3): the layer/region source registry, shared chrome
    /// material and dev load dial. The budget half lives on the `Dispatcher` (§2.2).
    pub(crate) glass: plx_ui::frame::glass::GlassPlan,
    /// **The container tree, and since phase 12 (D1) it is the ONE navigation authority.** It was
    /// a shadow through 3b and a half-real tree through 5b, kept in step with an `App.route` field
    /// by `bridge::sync_page` on every frame; both the field and the mirror are deleted. Every
    /// navigation is now a `NavOp` asked at the press that wants it (`app/bridge.rs`'s `nav_root`/
    /// `nav_push`/`nav_pop`/`nav_pop_to`/`nav_cancel`), the pages behind the one on screen are this
    /// stack's entries, and [`App::route`] is a one-line read of its top. The loop still hands it
    /// input and asks it to draw (`app/bridge.rs`'s coexistence contract).
    /// **It carries the ONE frame budget** (spec §2.2/§8.1): the frame scheduler owns admission,
    /// and there is one scheduler. `App` held a second `Budget` of its own until phase 11 — the
    /// poster upload spent that one while the dispatcher's step-8 present decision consulted the
    /// other, whose queue flag had no writer at all, so the two halves of one mechanism could
    /// never agree with each other.
    pub(crate) pages: plx_ui::dispatch::Dispatcher<bridge::AppHost>,
    /// Inputs collected for the dispatcher this iteration (`bridge` module doc).
    pub(crate) inputs: Vec<plx_machine::machine::InputEvent<u32>>,
    /// What that dispatcher borrows: the mounter, the real `TtfMeasure`, the store deliveries,
    /// the consent machine and the queue of requests an owned screen makes of this loop.
    pub(crate) bridge: bridge::Bridge,
}

impl App {
    /// **Which page is on top** (spec §15.2) — the container's answer, and since D1 the ONLY one.
    ///
    /// `App.route` was a `Route` field beside `App.trail`, `App.nav_pending` and `App.play_from`,
    /// kept in step with the tree by `bridge::sync_page` on every frame. All four are deleted:
    /// this is a one-line read of the entry the `NavStack` holds.
    ///
    /// The library root fallback covers exactly one moment — the frames before the first page is
    /// minted, which is boot.
    pub(crate) fn route(&self) -> AppArg {
        self.pages.top_arg().cloned().unwrap_or(AppArg::Local(plx_screens::registry::LocalArg { path: String::new() }))
    }
}

/// Everything `run_application` does before minting the main-thread token, in order.
fn pre_boot_diagnostics() {
    install_panic_logger();
    // WHICH INSTALL wrote this log, first, before anything can fail; the app directory with its
    // provenance on the next line.
    log(&format!(
        "install: id={} runtime={} features={}",
        plx_base::paths::app_id(),
        plx_base::paths::runtime_dir().display(),
        if plx_base::devtrig::ENABLED { "dev" } else { "release" },
    ));
    log(&plx_base::paths::app_dir_line());
    plx_platform::tv::probe_device();
    crate::dev::arm_wayland_debug();
    plx_platform::devcaps::probe();
    crate::dev::arm_gst_logging();
    plx_platform::tv::start_capability_probe();
}

/// The run+teardown sequence `run_application` hands the mounted `App` to.
unsafe fn run_and_shutdown(app: &mut App) -> c_int {
    run::run(app);
    app.glass.sources.borrow_mut().clear();
    run::shutdown(&mut app.player.session, &mut app.adapters.player);
    0
}

fn enter_application(pms_host: *const c_char, pms_port: c_int) -> Result<App,c_int> {
    pre_boot_diagnostics();
    let main_thread = unsafe { plx_base::task::MainThread::assume() };
    unsafe { boot(pms_host, pms_port, main_thread) }
}

/// The ten-line public skeleton: preflight/construction, then the ordinary loop and teardown.
pub(crate) fn run_application(pms_host: *const c_char, pms_port: c_int) -> c_int {
    let mut app = match enter_application(pms_host,pms_port) { Ok(app) => app, Err(code) => return code };
    unsafe { run_and_shutdown(&mut app) }
}


