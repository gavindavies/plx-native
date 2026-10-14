//! The video sink: the verbs the player engine speaks to the decoder. The product implements them
//! over GStreamer (`player::gst_sink`); the host tests over a clock (`player::ffi_host`).
//!
//! The verb set still has the shape of the buffer-feed pipeline the engine was written against:
//! a Load payload, `feed`, and the video-plane bind stages `pump` walks. A sink with no separate
//! video plane answers those stages as no-ops (`VP_EXPORTED`).
use plx_base::task::MainThread;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_long, c_uint};

/// Which video-plane binding the sink has.
pub const VP_NONE: c_int = 0;
#[allow(dead_code)]
pub const VP_ACB: c_int = 1;
pub const VP_EXPORTED: c_int = 2;

/// One method per verb. `&MainThread` on every method but `load` (the media worker calls it),
/// `window_mode` and `window_id` (memoized reads).
///
/// # Safety
/// A payload or id pointer must be a valid NUL-terminated string for the call, and `feed`'s
/// `p`/`size` a readable buffer. Apart from `load`, the verbs must be called on the main thread
/// (which `&MainThread` witnesses), in the order the player's pump drives them.
pub trait VideoSink: Sync {
    /// # Safety
    /// Unlike every other verb, `load` runs off the main thread (the media worker calls it, after
    /// `create_window` has put the windowId into `payload`), so an implementation must not touch
    /// main-thread state. `payload` must stay valid for the call.
    unsafe fn load(&self, payload: *const c_char, epoch: u32) -> c_int;
    unsafe fn ready(&self, mt: &MainThread) -> c_int;
    unsafe fn is_load_completed(&self, mt: &MainThread) -> c_int;
    unsafe fn play(&self, mt: &MainThread) -> c_int;
    unsafe fn pause(&self, mt: &MainThread) -> c_int;
    unsafe fn flush(&self, mt: &MainThread) -> c_int;
    unsafe fn push_eos(&self, mt: &MainThread) -> c_int;
    unsafe fn set_time_to_decode(&self, mt: &MainThread, position_ns: i64) -> c_int;
    unsafe fn set_content_info(&self, mt: &MainThread, position_ns: i64) -> c_int;
    unsafe fn send_segment(&self, mt: &MainThread) -> c_int;
    /// True when the library's `Feed` is the pre-C++11 copy-on-write-string build (webOS 3.x).
    /// A memoized read, so it takes no `MainThread`.
    fn feed_abi_is_cow(&self) -> bool;
    unsafe fn feed(&self, mt: &MainThread, p: *const u8, size: c_uint, pts: i64, es_data: c_int) -> c_char;
    unsafe fn unload(&self, mt: &MainThread);
    unsafe fn callback_gate_retire(&self, mt: &MainThread) -> c_int;
    unsafe fn callback_intercepts(&self, mt: &MainThread) -> u32;
    unsafe fn destroy(&self, mt: &MainThread) -> c_int;
    unsafe fn quarantine(&self, mt: &MainThread);
    fn window_mode(&self) -> c_int;
    /// The exported windowId the sink holds, or `""` when none was created. Never null. The
    /// storage is the sink's own long-lived buffer (the C seam's `g_window_id`), rewritten only by
    /// `create_window`/`destroy_window` on the main thread, so a caller copies it at once.
    fn window_id(&self) -> &'static CStr;
    /// # Safety
    /// Returns null when no window could be created (or the mode is not `VP_EXPORTED`); callers
    /// must check before reading the id. A non-null pointer is the sink's own buffer, valid until
    /// `destroy_window`.
    unsafe fn create_window(&self, mt: &MainThread) -> *const c_char;
    #[allow(clippy::too_many_arguments)]
    unsafe fn place_window(&self, mt: &MainThread, src_w: c_int, src_h: c_int, dst_x: c_int,
        dst_y: c_int, dst_w: c_int, dst_h: c_int) -> c_int;
    unsafe fn destroy_window(&self, mt: &MainThread);
    unsafe fn plane_create(&self, mt: &MainThread, app_id: *const c_char, player_type: c_int) -> c_long;
    unsafe fn plane_bind(&self, mt: &MainThread, media_id: *const c_char);
    unsafe fn plane_send_video_data(&self, mt: &MainThread, source_info: *const c_char) -> c_int;
    unsafe fn plane_send_atmos(&self, mt: &MainThread, media_id: *const c_char) -> c_int;
    unsafe fn plane_start(&self, mt: &MainThread, x: c_long, y: c_long, w: c_long, h: c_long);
    unsafe fn plane_unload(&self, mt: &MainThread);
    unsafe fn plane_pause(&self, mt: &MainThread);
    unsafe fn plane_resume(&self, mt: &MainThread);
}


