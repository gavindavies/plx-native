//! Rust port of src/system.c — webOS/SDL platform glue (system.h). Same C ABI.
//! sys_grab_wayland grabs the wayland surface and clears its opaque region so the
//! Starfish video plane below shows through. CRITICAL: the TV's SDL fork writes a
//! LARGER SDL_SysWMinfo than the headers declare, so SDL_GetWindowWMInfo is given
//! a generous over-allocated buffer — this overrun smashing a bare struct was the
//! modular-split crash. wl_surface opcode 4 = set_opaque_region.
use std::os::raw::{c_int, c_uint, c_void};

const SDL_GL_ALPHA_SIZE: c_int = 3; // SDL_GLattr: RED=0,GREEN=1,BLUE=2,ALPHA=3
/// `SDL_GL_DEPTH_SIZE` / `SDL_GL_STENCIL_SIZE` — same enum, 6 and 7. `app.rs` asks for zero of
/// both; these read back what the driver actually granted, which is the only thing that settles it.
const SDL_GL_DEPTH_SIZE: c_int = 6;
const SDL_GL_STENCIL_SIZE: c_int = 7;
const GL_ALPHA_BITS: c_uint = 0x0D55;
const GL_RED_BITS: c_uint = 0x0D52;
const GL_DEPTH_BITS: c_uint = 0x0D56;
const GL_STENCIL_BITS: c_uint = 0x0D57;
// SDL_SysWMinfo layout (32-bit): version u8[3]@0, subsystem int@4, info union@8

extern "C" {
    // the wayland grab is `cfg(not(hostsim))` — desktop SDL has no webOS surface to reach for
    #[allow(dead_code)]
    fn SDL_GetWindowWMInfo(window: *mut c_void, info: *mut c_void) -> c_int;
    /// Fills `SDL_version` — three `Uint8`, major/minor/patch.
    fn SDL_GetVersion(ver: *mut u8);
    fn SDL_GL_GetAttribute(attr: c_int, value: *mut c_int) -> c_int;
    fn glGetIntegerv(pname: c_uint, params: *mut c_int);
}

// The three symbols that exist only on a television: wayland's proxy marshaller and glib's main
// context. The desktop simulator links neither — SDL owns its own event loop there, and there is
// no luna bus to pump — so they are declared apart rather than in the block above, which would
// otherwise fail the host link.

static mut G_WL_SURFACE: *mut c_void = std::ptr::null_mut();
static mut G_WL_DISPLAY: *mut c_void = std::ptr::null_mut();

pub(crate) fn clear_opaque_region() {
    unsafe {
        let surface = G_WL_SURFACE;
        if surface.is_null() {
            return;
        }
        // set_opaque_region(NULL): opcode 4 + one NULL region arg (variadic). The
        // commit is left to SDL_GL_SwapWindow (a bare commit here presents a
        // null-buffer surface and disrupts the slaved video plane).
        //
        // Nothing to do on the simulator: there is no video plane underneath to show through, so
        // a non-opaque surface would buy a desktop compositor nothing but per-frame blending.
        //
        // …and nothing to LINK against under `cargo test`. Since phase 9 this is reached through
        // `Rig::clear_opaque_region` (`app/bridge.rs`), which the host suite exercises, where
        // before it was reachable only from `plex_run` and the dead-strip hid the missing
        // `libwayland-client`. The guard below is `G_WL_SURFACE`, which is null in a test, so
        // nothing is skipped that would have run.
    }
}

/// Null the Wayland surface and display pointers on app background.
///
/// SDL owns these client-side proxies; our references must not outlive an active window.
/// Background notifications revoke the borrow before any later frame can marshal through it.
/// This does not destroy SDL's objects. Foreground reacquires them from SDL, and resets the
/// opaque-region cache so a replacement surface receives its own request. Answers whether a
/// surface was held.
pub(crate) fn sys_release_wayland() -> bool {
    unsafe {
        let had_surface = !G_WL_SURFACE.is_null();
        G_WL_SURFACE = std::ptr::null_mut();
        G_WL_DISPLAY = std::ptr::null_mut();
        if had_surface {
            log("wm: released borrowed Wayland handles");
        }
        had_surface
    }
}

/// Service the glib main context that luna-service2 replies arrive on.
///
/// A no-op on the simulator: glib is not linked and there is no luna bus to pump, so the loop
/// simply has nothing to service. Every caller stays unchanged.
pub(crate) fn ls2_pump() {
}

pub(crate) fn sys_grab_wayland(winp: *mut c_void) {
    unsafe {
        let mut wmbuf = [0u8; 512];
        // SDL_VERSION(&wm->version): major/minor/patch (u8) at offset 0.
        //
        // ASK THE LIBRARY, do not hardcode. This declares which SDL_SysWMinfo layout the CALLER
        // was built against, and SDL checks it: from 2.0.6 on, `Wayland_GetWindowWMInfo` computes
        // major*1000000 + minor*10000 + patch and, if that is below 2000006, sets
        // subsystem = SDL_SYSWM_UNKNOWN and returns failure WITHOUT filling the union. This used
        // to say 2/0/4 — the version webOS 4.x ships — which is the right answer on exactly the
        // televisions the app has run on and wrong on every newer one: webOS 5.3.1/6.4.0 ship
        // SDL 2.0.10 and 7.4.0+ ship 2.0.14.
        //
        // The failure mode is why this matters more than it looks. A rejected call leaves
        // `G_WL_SURFACE` null, so `clear_opaque_region` silently does nothing, so the UI plane
        // stays opaque — and video decodes correctly, invisibly, underneath it. No error, no log
        // line, no crash: a black screen with working audio.
        //
        // Reporting the runtime version is safe in the other direction because the union's first
        // two members — `wl_display *display` then `wl_surface *surface` — have never moved;
        // later SDL versions only APPEND to that struct, which is also why the buffer is
        // over-allocated to 512 bytes (the TV's fork writes more than the headers declare).
        SDL_GetVersion(wmbuf.as_mut_ptr());
        let mut a: c_int = -1;
        let mut d: c_int = -1;
        let mut s: c_int = -1;
        SDL_GL_GetAttribute(SDL_GL_ALPHA_SIZE, &mut a);
        SDL_GL_GetAttribute(SDL_GL_DEPTH_SIZE, &mut d);
        SDL_GL_GetAttribute(SDL_GL_STENCIL_SIZE, &mut s);
        let mut abits: c_int = -1;
        let mut rbits: c_int = -1;
        let mut dbits: c_int = -1;
        let mut sbits: c_int = -1;
        glGetIntegerv(GL_ALPHA_BITS, &mut abits);
        glGetIntegerv(GL_RED_BITS, &mut rbits);
        // Deprecated in a desktop CORE profile (the simulator), where they leave the value alone
        // and raise `GL_INVALID_ENUM` — harmless and once, at boot, and the SDL attributes above
        // answer the same question portably. On the television's ES2 context both are legal.
        glGetIntegerv(GL_DEPTH_BITS, &mut dbits);
        glGetIntegerv(GL_STENCIL_BITS, &mut sbits);
        // `depth=`/`stencil=` are here to be READ: `app.rs` asks for zero of each because nothing
        // in this renderer uses them, and on a tiler a granted depth buffer is a per-frame
        // write-back of 1920x1080x2 bytes nobody consumes. A non-zero here means the driver
        // refused and the saving is not real.
        log(&format!(
            "FB bits: alpha={abits} red={rbits} depth={dbits} stencil={sbits} \
             (config alpha={a} depth={d} stencil={s})"
        ));
        // The Wayland query is webOS-only. Desktop SDL reports a different backend with a
        // different union layout, and has no hardware video plane needing this request.
        // update_wayland_info reads unaligned because the oversized byte buffer has no pointer
        // alignment guarantee, and rejects any non-Wayland subsystem before reading the union.
        let _ = winp;
        let subsystem = i32::from_ne_bytes([wmbuf[4], wmbuf[5], wmbuf[6], wmbuf[7]]);
        let (surf, disp) = (G_WL_SURFACE, G_WL_DISPLAY);
        // The version bytes are still in wmbuf[0..3] — SDL_GetWindowWMInfo validates them and
        // writes only `subsystem` and the union, so there is nothing to re-read.
        log(&format!(
            "wm sdl={}.{}.{} subsys={subsystem} wl_surface={surf:p} wl_display={disp:p} alpha={a}",
            wmbuf[0], wmbuf[1], wmbuf[2]
        ));
        // Loud, because the consequence is a black screen with working audio and nothing else in
        // the log would say why. SDL_SYSWM_WAYLAND is 6 in SDL2's enum; anything else here means
        // we did not get a surface and the video plane will stay hidden under an opaque UI.
        if surf.is_null() && !true {
            log(
                "wm: NO wl_surface — the UI plane cannot be made transparent, so video will \
                 decode invisibly beneath it. Check the SDL_SysWMinfo version handshake.",
            );
        }
        clear_opaque_region();
    }
}

use plx_base::eventlog::log;

// ------------------------------------------------------------------------------------------
// EXPERIMENT (`/tmp/pitv-opaque`): declare the UI surface OPAQUE where nothing is behind it
// ------------------------------------------------------------------------------------------
//
// `docs/perf-damage-tracking-verdict.md` §5 is the design. The whole app has run with
// `set_opaque_region(NULL)` since boot — on Home, Library, Detail, Person, Login and Profiles,
// where there is no video plane underneath and nothing to show through. The protocol calls the
// opaque region "an optimization hint for the compositor that lets it optimize the redrawing of
// content behind opaque regions", and a sibling measured LG's `surface-manager` at 34.4% of every
// frame's GPU cycles, doing exactly one full-screen textured blit of our surface. Whether that
// charge is per-COMMIT bookkeeping or per-BLENDED-PIXEL is the question, and nobody has varied the
// variable: all four prior compositor measurements held the surface non-opaque at 1920x1080.
//
// **Default behaviour is byte-identical.** Without the trigger `region_init` never runs, `ENABLED`
// stays false, and `opaque_route` returns on its first load.
//
// **Plane-scoped, and it must be.** Marking the surface opaque while the hardware video plane is
// bound beneath it would occlude a plane that has not torn down yet — which is why this reasserts
// NULL every frame while the plane is bound. `opaque_route(video_plane_bound)` therefore asserts
// NULL while the plane is bound and the full region otherwise (a pre-bind spinner or a post-unbind
// read-out gets the opaque region back), and remembers which it last sent so an unchanged state
// costs one atomic load and no protocol traffic (the region is double-buffered but otherwise
// STICKY: "the pending and current regions are never changed" otherwise).
//
// **No new link dependency, deliberately.** A `wl_region` needs `wl_compositor.create_region`, and
// the only wayland objects this app has are the display and surface SDL handed it — so the
// compositor has to come from a registry bind. `wl_proxy_marshal_constructor_versioned` is absent
// from `libwayland-client` on webOS below 4.4.2 and the `*_interface` data symbols would be three
// more `DT_NEEDED` requirements, so every one of them is resolved through `RTLD_DEFAULT` against
// the libwayland-client already linked and loaded. `tools/fwcompat.py` is unchanged by this file.








pub(crate) fn opaque_region_init() {}


/// Nothing to declare opaque on a desktop: there is no video plane underneath and no LG compositor
/// to hint. The simulator keeps the same call site rather than growing a `cfg` at it.
pub(crate) fn opaque_route(_player: bool) {}

// ---- The COMPOSITOR FRAME-CALLBACK PROBE (`/tmp/pitv-framecb`) -----------------------------
//
// A present's wait for a free back buffer is paid inside the frame's first framebuffer-0 command
// (the `clear` span), and from inside the app that wait reads the same whether the compositor
// repainted late, skipped a repaint, or repainted on time and we were slow to notice. This probe
// asks the compositor directly: one `wl_surface.frame` request per presented frame, placed before
// the swap that commits it, whose `done(time)` event carries the COMPOSITOR'S OWN millisecond
// stamp of the repaint that picked the commit up. Printed beside the frame's `FRAMEDROP` line
// with our own monotonic clock, the two tell those cases apart.
//
// It also prices the frame thread over the three stretches of a frame's present: `w` (the
// first framebuffer command, where the driver waits for a back buffer), `d` (the draw after it)
// and `s` (the swap). Each prints `<p>=<wall>/<cpu>/<runq>` in ms and `<p>sw=<slept>/<preempted>`
// context switches: wall time that is neither CPU nor run-queue wait was spent BLOCKED, run-queue
// wait is a thread that wanted to run and was not given a CPU, and CPU is work. `wait_at=` is the
// monotonic ms at which the wait began, for lining a frame up with a kernel trace.
//
// A fourth stretch, `p`, prices the frame loop's `prepare_window` (the `prepare=` stage): it runs
// BEFORE the back-buffer wait, on every frame, presenting or not, so it has its own pair of marks
// (`frame_probe_prepare_begin`/`_end`) apart from the wait/commit slots. Each frame's begin
// overwrites the pair, so a value is never carried into a later frame's line; it prints only when
// both marks of the frame that is being reported were taken.
//
// Same linkage rule as the opaque experiment above: every libwayland entry point is resolved
// from the process's own scope, ONCE (`frame_syms`, on the first armed present, before the swap
// stretch is marked), so the probe adds no symbol to the link and its lookups are not charged to
// the `s` stretch. Unarmed it is one latched bool per presented frame. The whole probe is
// compiled only with `devtriggers` (and not in the simulator); the shipping build has the empty
// stubs at the end of this section.

















pub(crate) fn frame_probe_request() {}
pub(crate) fn frame_probe_prepare_begin() {}
pub(crate) fn frame_probe_prepare_end() {}
pub(crate) fn frame_probe_waiting() {}
pub(crate) fn frame_probe_acquired() {}
pub(crate) fn frame_probe_fields() -> String {
    String::new()
}

// Publish SDL's borrowed handles. Kept separate from the native query so failed queries can
// be exercised without loading the television's SDL or marshalling a fake proxy.
#[cfg(test)]
unsafe fn update_wayland_info(ok: c_int, info: &[u8; 512]) {
    sys_release_wayland();
    let subsystem = i32::from_ne_bytes(info[4..8].try_into().unwrap());
    let (mut display_present, mut surface_present) = (false, false);
    if ok != 0 && subsystem == 6 { // SDL_SYSWM_WAYLAND
        let pointers = info.as_ptr().add(8) as *const *mut c_void;
        let display = pointers.read_unaligned();
        let surface = pointers.add(1).read_unaligned();
        display_present = !display.is_null();
        surface_present = !surface.is_null();
        if display_present && surface_present {
            G_WL_DISPLAY = display;
            G_WL_SURFACE = surface;
        }
    }
    let stage = if ok == 0 { "query failed" }
        else if subsystem != 6 { "not a Wayland window" }
        else if G_WL_SURFACE.is_null() { "no surface" }
        else { "ready" };
    plx_base::eventlog::log(&format!(
        "window: wayland {stage} (SDL {}.{}.{}, display={display_present}, surface={surface_present})",
        info[0], info[1], info[2]
    ));
}

#[cfg(test)]
mod wayland_tests {
    use super::*;

    #[test]
    fn failed_refresh_cannot_reuse_a_previous_surface() {
        let _guard = plx_base::testlock::serial();
        unsafe {
            let mut display = 0u8;
            let mut surface = 0u8;
            G_WL_DISPLAY = std::ptr::addr_of_mut!(display).cast();
            G_WL_SURFACE = std::ptr::addr_of_mut!(surface).cast();
            update_wayland_info(0, &[0; 512]);
            let (display, surface) = (G_WL_DISPLAY, G_WL_SURFACE);
            G_WL_DISPLAY = std::ptr::null_mut();
            G_WL_SURFACE = std::ptr::null_mut();
            assert!(display.is_null() && surface.is_null(),
                "a failed SDL query must revoke both borrowed handles");
        }
    }

    #[test]
    fn background_release_and_foreground_refresh_replace_the_borrow() {
        let _guard = plx_base::testlock::serial();
        unsafe {
            let mut display = 0u8;
            let mut surface = 0u8;
            let display_ptr = std::ptr::addr_of_mut!(display).cast::<c_void>();
            let surface_ptr = std::ptr::addr_of_mut!(surface).cast::<c_void>();
            let mut info = [0u8; 512];
            info[4..8].copy_from_slice(&6i32.to_ne_bytes());
            let pointers = info.as_mut_ptr().add(8) as *mut *mut c_void;
            pointers.write_unaligned(display_ptr);
            pointers.add(1).write_unaligned(surface_ptr);
            update_wayland_info(1, &info);
            let acquired = (G_WL_DISPLAY, G_WL_SURFACE);
            assert!(sys_release_wayland(), "a held surface answers true");
            assert!(!sys_release_wayland(), "WILL + DID background is idempotent: nothing left to release");
            let released = (G_WL_DISPLAY, G_WL_SURFACE);
            clear_opaque_region(); // No native marshal can run after revocation.
            let mut replacement = 0u8;
            let replacement_ptr = std::ptr::addr_of_mut!(replacement).cast::<c_void>();
            pointers.add(1).write_unaligned(replacement_ptr);
            update_wayland_info(1, &info);
            let refreshed = (G_WL_DISPLAY, G_WL_SURFACE);
            sys_release_wayland();
            assert_eq!(acquired, (display_ptr, surface_ptr));
            assert!(released.0.is_null() && released.1.is_null());
            assert_eq!(refreshed, (display_ptr, replacement_ptr));
        }
    }

    #[test]
    fn foreign_or_incomplete_wm_info_cannot_supply_wayland_handles() {
        let _guard = plx_base::testlock::serial();
        unsafe {
            for subsystem in [0i32, 4, 6] {
                let mut info = [0u8; 512];
                info[4..8].copy_from_slice(&subsystem.to_ne_bytes());
                // A foreign backend with two non-null pointers, or Wayland with no surface.
                let mut display = 0u8;
                let mut surface = 0u8;
                let pointers = info.as_mut_ptr().add(8) as *mut *mut c_void;
                pointers.write_unaligned(std::ptr::addr_of_mut!(display).cast());
                if subsystem != 6 {
                    pointers.add(1).write_unaligned(std::ptr::addr_of_mut!(surface).cast());
                }
                update_wayland_info(1, &info);
                let handles = (G_WL_DISPLAY, G_WL_SURFACE);
                sys_release_wayland();
                assert!(handles.0.is_null() && handles.1.is_null());
            }
        }
    }
}
