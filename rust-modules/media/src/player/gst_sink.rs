//! **The Linux build's player: the fed access units, decoded by the system GStreamer.**
//!
//! The television hands the demuxer's access units to LG's StarfishMediaAPIs, which is itself a
//! GStreamer pipeline behind a buffer-feed API. This is the same shape built in the open: one
//! `appsrc` per lane (`es_data` 1 = video, 2 = audio), the codec's parser, `decodebin` (which
//! ranks a hardware decoder — V4L2 on a Raspberry Pi, VA-API on a PC — above software when the
//! machine has one), then
//!
//! * video → `videoconvert ! videoscale` to straight RGBA no larger than 1920x1080 → an `appsink`
//!   synced to the pipeline clock, whose frames the render thread uploads and composites UNDER
//!   the finished UI with the same `(ONE_MINUS_DST_ALPHA, ONE)` arithmetic the television's
//!   compositor applies to its video plane ([`composite_under`]);
//! * audio → decoded to PCM → `audioconvert ! audioresample` → the system audio sink
//!   (`autoaudiosink`, or `PLXNATIVE_GST_AUDIOSINK`), which also provides the clock both lanes
//!   present against.
//!
//! # The contract it keeps
//!
//! Everything above the [`VideoSink`] seam is the television's code, unchanged, so this sink
//! speaks the seam's dialect rather than GStreamer's: `Load` acknowledges with the `loadCompleted`
//! callback, the position arrives as callback type 0 carrying the FED timestamp at the pipeline's
//! own 5 Hz cadence, a pipeline error before any picture is type 18, and `Unload` confirms with
//! type 23 — all through [`super::sf_on_event`], the same entry LG's library thread calls, so the
//! `pts_shift`/`disp_base` rebase the engine applies is exercised exactly as on a set.
//!
//! It reports `VP_EXPORTED`, the webOS 5+ video path: a seek is then a fresh `Load`, so each Load
//! builds a fresh pipeline and the fed timeline always starts at that Load's first buffer. GStreamer
//! wants a segment that starts where the data does, so the first timestamp fed is the session's
//! `base` and every buffer is pushed at `pts - base`; positions are reported back as
//! `position + base`. An audio frame fed before that first buffer is dropped (it would sit before
//! the segment) — the demuxer feeds the video keyframe it opened on first.
//!
//! Backpressure is real here, unlike the clock sink's: a lane whose `appsrc` already holds its
//! byte budget answers the feed with `'B'`, and the pump retries the same access unit later.

use plx_base::task::MainThread;
use plx_platform::tv::sink::VideoSink;
use std::ffi::{c_uint, CStr};
use std::os::raw::{c_char, c_int, c_long};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

const FEED_OK: c_char = b'O' as c_char;
const FEED_FULL: c_char = b'B' as c_char;
const FEED_ERROR: c_char = b'e' as c_char;

/// Position cadence, milliseconds: the television pipeline's own 5 Hz (see `ffi_host::TICK_MS`).
const TICK_MS: u64 = 200;
/// Each lane's `appsrc` budget. Video holds a couple of seconds of a high-bitrate stream; the AU
/// queues upstream are the real reserve, these only keep the decoder fed.
const VIDEO_QUEUE_BYTES: u64 = 24 * 1024 * 1024;
const AUDIO_QUEUE_BYTES: u64 = 2 * 1024 * 1024;
/// The largest picture handed to the render thread. A 4K source is scaled here, in the pipeline,
/// rather than uploaded at 33 MB a frame.
const MAX_W: i32 = 1920;
const MAX_H: i32 = 1080;

/// One Load's pipeline.
struct Session {
    pipeline: gst::Pipeline,
    video: gst_app::AppSrc,
    audio: Option<gst_app::AppSrc>,
    /// The first fed timestamp, `i64::MIN` until one arrives. Shared with the ticker.
    base: Arc<AtomicI64>,
    generation: u64,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);
/// Bumped whenever the session is replaced or dropped, so its ticker thread stops.
static GENERATION: AtomicU64 = AtomicU64::new(0);

// The seam lifecycle, with the same meaning as the host sink's statics of the same names.
static OBJECT_READY: AtomicBool = AtomicBool::new(false);
static LOADED: AtomicBool = AtomicBool::new(false);
static PLAYING: AtomicBool = AtomicBool::new(false);
static ACTIVE_EPOCH: AtomicU32 = AtomicU32::new(0);
static CALLBACK_INTERCEPTS: AtomicU32 = AtomicU32::new(0);
static CALLBACK_GATE_RETIRED: AtomicBool = AtomicBool::new(false);
static NATIVE_UNLOAD_COMPLETED: AtomicBool = AtomicBool::new(false);
static LIFECYCLE_BLOCKED: AtomicBool = AtomicBool::new(false);
/// A picture of this session has been presented; a pipeline error after it is not a refusal.
static PRESENTED: AtomicBool = AtomicBool::new(false);

fn log(s: &str) {
    plx_base::eventlog::log(&format!("gst: {s}"));
}

fn gst_ready() -> bool {
    static INIT: OnceLock<bool> = OnceLock::new();
    *INIT.get_or_init(|| match gst::init() {
        Ok(()) => {
            let (major, minor, micro, _) = gst::version();
            log(&format!("GStreamer {major}.{minor}.{micro} initialised"));
            true
        }
        Err(e) => {
            log(&format!("GStreamer could not initialise ({e}); no playback"));
            false
        }
    })
}

/// Deliver one seam callback for the current session, counted like a firmware callback.
fn callback(ty: c_int, num: i64, s: &CStr) {
    let epoch = ACTIVE_EPOCH.load(Relaxed);
    if epoch != 0 && !CALLBACK_GATE_RETIRED.load(Relaxed) {
        CALLBACK_INTERCEPTS.fetch_add(1, Relaxed);
        super::sf_on_event(epoch, ty, num, s.as_ptr());
    }
}

// ---- the Load payload -------------------------------------------------------------------------

/// The value of `"key":"<value>"` in the Load payload. The payload is the engine's own template
/// with substitutions (`engine::build_av_payload`), so a string search is exact here.
fn payload_str<'a>(payload: &'a str, key: &str) -> Option<&'a str> {
    let anchor = format!("\"{key}\":\"");
    let start = payload.find(&anchor)? + anchor.len();
    let len = payload[start..].find('"')?;
    Some(&payload[start..start + len])
}

/// `(appsrc caps, parser)` for the payload's video codec.
fn video_branch(codec: &str) -> Option<(&'static str, &'static str)> {
    Some(match codec {
        "H264" => ("video/x-h264,stream-format=byte-stream,alignment=au", "h264parse"),
        "H265" => ("video/x-h265,stream-format=byte-stream,alignment=au", "h265parse"),
        "AV1" => ("video/x-av1,stream-format=obu-stream,alignment=tu", "av1parse"),
        "VP9" => ("video/x-vp9", "vp9parse"),
        _ => return None,
    })
}

/// `(appsrc caps, parser)` for the payload's audio codec, in LG's vocabulary
/// (`engine::audio_payload_codec`). The demuxer frames AAC as ADTS for exactly this reason.
fn audio_branch(codec: &str) -> Option<(&'static str, &'static str)> {
    Some(match codec {
        "AC3" => ("audio/x-ac3,framed=true", "ac3parse"),
        "AC3 PLUS" => ("audio/x-eac3,framed=true", "ac3parse"),
        "AAC" => ("audio/mpeg,mpegversion=4,stream-format=adts", "aacparse"),
        "DTS" => ("audio/x-dts,framed=true", "dcaparse"),
        _ => return None,
    })
}

/// The `gst-launch` description of the whole pipeline: the video lane always, the audio lane when
/// the payload declares one.
fn describe(video: (&str, &str), audio: Option<(&str, &str)>, audio_sink: &str) -> String {
    let mut d = format!(
        "appsrc name=v format=time is-live=false stream-type=stream block=false max-bytes={VIDEO_QUEUE_BYTES} caps=\"{}\" \
         ! {} ! decodebin ! videoconvert ! videoscale add-borders=true \
         ! video/x-raw,format=RGBA,pixel-aspect-ratio=1/1,width=[1,{MAX_W}],height=[1,{MAX_H}] \
         ! appsink name=vsink sync=true max-buffers=2 drop=false",
        video.0, video.1
    );
    if let Some((caps, parser)) = audio {
        d.push_str(&format!(
            " appsrc name=a format=time is-live=false stream-type=stream block=false max-bytes={AUDIO_QUEUE_BYTES} caps=\"{caps}\" \
             ! {parser} ! decodebin ! audioconvert ! audioresample ! {audio_sink}"
        ));
    }
    d
}

fn audio_sink_description() -> String {
    std::env::var("PLXNATIVE_GST_AUDIOSINK")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "autoaudiosink".to_owned())
}

// ---- frames for the render thread -------------------------------------------------------------

struct Frame {
    seq: u64,
    w: i32,
    h: i32,
    rgba: Arc<Vec<u8>>,
}

static LATEST: Mutex<Option<Frame>> = Mutex::new(None);
static FRAME_SEQ: AtomicU64 = AtomicU64::new(0);
/// Where the engine placed the video (`place_window`), in UI pixels; all zero = the whole screen.
static DST: Mutex<[i32; 4]> = Mutex::new([0; 4]);

fn publish_frame(sample: &gst::Sample) -> Result<(), gst::FlowError> {
    let caps = sample.caps().ok_or(gst::FlowError::Error)?;
    let s = caps.structure(0).ok_or(gst::FlowError::Error)?;
    let (w, h) = (s.get::<i32>("width").map_err(|_| gst::FlowError::Error)?, s.get::<i32>("height").map_err(|_| gst::FlowError::Error)?);
    let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
    let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
    let need = (w as usize) * (h as usize) * 4;
    if map.len() < need {
        return Err(gst::FlowError::Error);
    }
    let seq = FRAME_SEQ.fetch_add(1, Relaxed) + 1;
    *LATEST.lock().unwrap_or_else(|e| e.into_inner()) = Some(Frame { seq, w, h, rgba: Arc::new(map[..need].to_vec()) });
    if !PRESENTED.swap(true, Relaxed) {
        log(&format!("first picture {w}x{h}"));
    }
    plx_machine::idle::invalidate();
    Ok(())
}

fn clear_picture() {
    *LATEST.lock().unwrap_or_else(|e| e.into_inner()) = None;
    plx_machine::idle::invalidate();
}

/// `(x, y, w, h)` of a `fw`x`fh` picture fitted, aspect kept, inside the `dst` rectangle.
fn letterbox(fw: i32, fh: i32, dst: [i32; 4]) -> (f32, f32, f32, f32) {
    let [dx, dy, dw, dh] = dst.map(|v| v as f32);
    if fw <= 0 || fh <= 0 || dw <= 0.0 || dh <= 0.0 {
        return (dx, dy, dw, dh);
    }
    let scale = (dw / fw as f32).min(dh / fh as f32);
    let (w, h) = (fw as f32 * scale, fh as f32 * scale);
    (dx + (dw - w) / 2.0, dy + (dh - h) / 2.0, w, h)
}

/// The render thread's half: upload the newest decoded picture if it changed and composite it
/// under what the UI drew this frame. Called once per presented frame while the player is up.
pub fn composite_under() {
    thread_local! {
        /// `(texture, frame number it holds)`.
        static TEX: std::cell::Cell<(c_uint, u64)> = const { std::cell::Cell::new((0, 0)) };
    }
    let guard = LATEST.lock().unwrap_or_else(|e| e.into_inner());
    let Some(frame) = guard.as_ref() else { return };
    let (seq, w, h, rgba) = (frame.seq, frame.w, frame.h, Arc::clone(&frame.rgba));
    drop(guard);
    let mut dst = *DST.lock().unwrap_or_else(|e| e.into_inner());
    if dst[2] <= 0 || dst[3] <= 0 {
        dst = [0, 0, 1920, 1080];
    }
    TEX.with(|t| {
        let (mut tex, held) = t.get();
        if held != seq {
            tex = plx_gfx::gfx::upload_rgba(tex, w, h, rgba.as_ptr());
            t.set((tex, seq));
        }
        let (x, y, dw, dh) = letterbox(w, h, dst);
        plx_gfx::gfx::draw_under(tex, x, y, dw, dh);
    });
}

// ---- the session ------------------------------------------------------------------------------

fn build_session(payload: &str) -> Result<Session, String> {
    let vcodec = payload_str(payload, "video").unwrap_or("H264");
    let video = video_branch(vcodec).ok_or_else(|| format!("no GStreamer branch for video codec {vcodec:?}"))?;
    let audio = match payload_str(payload, "audio") {
        None | Some("") => None,
        Some(a) => Some(audio_branch(a).ok_or_else(|| format!("no GStreamer branch for audio codec {a:?}"))?),
    };
    let description = describe(video, audio, &audio_sink_description());
    let pipeline = gst::parse::launch(&description)
        .map_err(|e| format!("pipeline did not build: {e}"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "pipeline description did not yield a pipeline".to_owned())?;
    let appsrc = |name: &str| {
        pipeline
            .by_name(name)
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .ok_or_else(|| format!("no appsrc {name}"))
    };
    let video_src = appsrc("v")?;
    let audio_src = if audio.is_some() { Some(appsrc("a")?) } else { None };
    let sink = pipeline
        .by_name("vsink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or("no appsink")?;
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(|sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                publish_frame(&sample)?;
                Ok(gst::FlowSuccess::Ok)
            })
            .new_preroll(|sink| {
                // The paused picture: a Load that has not been told to Play yet still shows its
                // first frame, as the television does once its pipeline has prerolled.
                if let Ok(sample) = sink.pull_preroll() {
                    let _ = publish_frame(&sample);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    log(&format!("pipeline v={vcodec} a={}", payload_str(payload, "audio").unwrap_or("none")));
    Ok(Session {
        pipeline,
        video: video_src,
        audio: audio_src,
        base: Arc::new(AtomicI64::new(i64::MIN)),
        generation: GENERATION.fetch_add(1, Relaxed) + 1,
    })
}

/// The session's own thread: the 5 Hz position report and the bus.
fn start_ticker(pipeline: gst::Pipeline, base: Arc<AtomicI64>, generation: u64) {
    let spawned = std::thread::Builder::new().name("gstsink".into()).spawn(move || {
        let bus = match pipeline.bus() {
            Some(b) => b,
            None => return,
        };
        while GENERATION.load(Relaxed) == generation {
            let wait = gst::ClockTime::from_mseconds(TICK_MS);
            if let Some(msg) = bus.timed_pop_filtered(wait, &[gst::MessageType::Error, gst::MessageType::Eos, gst::MessageType::Warning]) {
                if GENERATION.load(Relaxed) != generation {
                    break;
                }
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        let what = format!("{} ({:?})", e.error(), e.debug());
                        log(&format!("pipeline ERROR: {what}"));
                        if !PRESENTED.load(Relaxed) {
                            // Before any picture this is the television's asynchronous refusal.
                            if let Ok(c) = std::ffi::CString::new(e.error().to_string()) {
                                callback(18, 1, &c);
                            }
                        }
                    }
                    gst::MessageView::Warning(w) => log(&format!("pipeline warning: {}", w.error())),
                    gst::MessageView::Eos(_) => log("end of stream"),
                    _ => {}
                }
                continue;
            }
            if !PLAYING.load(Relaxed) || !LOADED.load(Relaxed) {
                continue;
            }
            let b = base.load(Relaxed);
            if b == i64::MIN {
                continue;
            }
            if let Some(pos) = pipeline.query_position::<gst::ClockTime>() {
                callback(0, b.saturating_add(pos.nseconds() as i64), c"");
            }
        }
    });
    if spawned.is_err() {
        log("could not spawn the position thread; no position will report");
    }
}

fn drop_session() {
    GENERATION.fetch_add(1, Relaxed);
    if let Some(s) = SESSION.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = s.pipeline.set_state(gst::State::Null);
    }
    PLAYING.store(false, Relaxed);
    PRESENTED.store(false, Relaxed);
}

fn with_session<T>(f: impl FnOnce(&Session) -> T) -> Option<T> {
    SESSION.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(f)
}

unsafe fn sf_load(payload: *const c_char, epoch: u32) -> c_int {
    if epoch == 0 || OBJECT_READY.load(Relaxed) || LIFECYCLE_BLOCKED.load(Relaxed) || !gst_ready() {
        return 0;
    }
    let payload = if payload.is_null() { String::new() } else { CStr::from_ptr(payload).to_string_lossy().into_owned() };
    drop_session();
    clear_picture();
    let session = match build_session(&payload) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("Load refused: {e}"));
            return 0;
        }
    };
    if session.pipeline.set_state(gst::State::Paused).is_err() {
        log("Load refused: the pipeline would not pause");
        let _ = session.pipeline.set_state(gst::State::Null);
        return 0;
    }
    start_ticker(session.pipeline.clone(), Arc::clone(&session.base), session.generation);
    *SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(session);
    ACTIVE_EPOCH.store(epoch, Relaxed);
    CALLBACK_INTERCEPTS.store(0, Relaxed);
    CALLBACK_GATE_RETIRED.store(false, Relaxed);
    NATIVE_UNLOAD_COMPLETED.store(false, Relaxed);
    OBJECT_READY.store(true, Relaxed);
    LOADED.store(true, Relaxed);
    callback(2, 0, c"{\"loadCompleted\":true}");
    1
}

fn set_state(state: gst::State) -> c_int {
    c_int::from(with_session(|s| s.pipeline.set_state(state).is_ok()).unwrap_or(false))
}

unsafe fn sf_feed(p: *const u8, size: c_uint, pts: i64, es_data: c_int) -> c_char {
    let guard = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = guard.as_ref() else { return FEED_ERROR };
    if p.is_null() || size == 0 {
        return FEED_OK;
    }
    let (src, budget) = match es_data {
        1 => (&s.video, VIDEO_QUEUE_BYTES),
        2 => match &s.audio {
            Some(a) => (a, AUDIO_QUEUE_BYTES),
            None => return FEED_OK,
        },
        _ => return FEED_OK,
    };
    if src.current_level_bytes() >= budget {
        return FEED_FULL;
    }
    let base = match s.base.compare_exchange(i64::MIN, pts, Relaxed, Relaxed) {
        Ok(_) => pts,
        Err(b) => b,
    };
    if pts < base {
        return FEED_OK; // before the segment's start; see the module doc
    }
    let mut buffer = gst::Buffer::from_mut_slice(std::slice::from_raw_parts(p, size as usize).to_vec());
    if let Some(b) = buffer.get_mut() {
        b.set_pts(gst::ClockTime::from_nseconds((pts - base) as u64));
    }
    match src.push_buffer(buffer) {
        Ok(_) => FEED_OK,
        Err(gst::FlowError::Flushing) => FEED_OK,
        Err(e) => {
            log(&format!("feed es={es_data} refused: {e:?}"));
            FEED_ERROR
        }
    }
}

unsafe fn sf_flush() -> c_int {
    // A seek on this path is a fresh Load, so a flush restarts the fed timeline: drop what is
    // queued and decoded, and take the next buffer as the new base.
    with_session(|s| {
        let flushed = s.pipeline.send_event(gst::event::FlushStart::new()) && s.pipeline.send_event(gst::event::FlushStop::new(true));
        s.base.store(i64::MIN, Relaxed);
        c_int::from(flushed)
    })
    .unwrap_or(0)
}

unsafe fn sf_push_eos() -> c_int {
    with_session(|s| {
        let _ = s.video.end_of_stream();
        if let Some(a) = &s.audio {
            let _ = a.end_of_stream();
        }
        1
    })
    .unwrap_or(0)
}

unsafe fn sf_unload() {
    let epoch = ACTIVE_EPOCH.load(Relaxed);
    drop_session();
    clear_picture();
    if epoch != 0 && OBJECT_READY.load(Relaxed) {
        NATIVE_UNLOAD_COMPLETED.store(true, Relaxed);
        super::sf_on_event(epoch, 23, 0, std::ptr::null());
    }
    LOADED.store(false, Relaxed);
}

unsafe fn sf_destroy() -> c_int {
    let safe = OBJECT_READY.load(Relaxed)
        && CALLBACK_GATE_RETIRED.load(Relaxed)
        && CALLBACK_INTERCEPTS.load(Relaxed) != 0
        && NATIVE_UNLOAD_COMPLETED.load(Relaxed)
        && !LIFECYCLE_BLOCKED.load(Relaxed);
    if !safe {
        sf_quarantine();
        return 0;
    }
    drop_session();
    clear_picture();
    ACTIVE_EPOCH.store(0, Relaxed);
    OBJECT_READY.store(false, Relaxed);
    LOADED.store(false, Relaxed);
    1
}

unsafe fn sf_quarantine() {
    drop_session();
    clear_picture();
    CALLBACK_GATE_RETIRED.store(true, Relaxed);
    LIFECYCLE_BLOCKED.store(true, Relaxed);
    OBJECT_READY.store(false, Relaxed);
    LOADED.store(false, Relaxed);
}

/// The Linux build's [`VideoSink`]. Installed by the application's `port.rs` under `gst`.
pub struct GstSink;

impl VideoSink for GstSink {
    unsafe fn load(&self, payload: *const c_char, epoch: u32) -> c_int {
        sf_load(payload, epoch)
    }
    unsafe fn ready(&self, _: &MainThread) -> c_int {
        c_int::from(OBJECT_READY.load(Relaxed))
    }
    unsafe fn is_load_completed(&self, _: &MainThread) -> c_int {
        c_int::from(LOADED.load(Relaxed))
    }
    unsafe fn play(&self, _: &MainThread) -> c_int {
        let r = set_state(gst::State::Playing);
        if r == 1 {
            PLAYING.store(true, Relaxed);
        }
        r
    }
    unsafe fn pause(&self, _: &MainThread) -> c_int {
        let r = set_state(gst::State::Paused);
        if r == 1 {
            PLAYING.store(false, Relaxed);
        }
        r
    }
    unsafe fn flush(&self, _: &MainThread) -> c_int {
        sf_flush()
    }
    unsafe fn push_eos(&self, _: &MainThread) -> c_int {
        sf_push_eos()
    }
    // Segment re-anchoring belongs to the in-place seek, which `VP_EXPORTED` never takes.
    unsafe fn set_time_to_decode(&self, _: &MainThread, _position_ns: i64) -> c_int {
        c_int::from(LOADED.load(Relaxed))
    }
    unsafe fn set_content_info(&self, _: &MainThread, _position_ns: i64) -> c_int {
        c_int::from(LOADED.load(Relaxed))
    }
    unsafe fn send_segment(&self, _: &MainThread) -> c_int {
        c_int::from(LOADED.load(Relaxed))
    }
    fn feed_abi_is_cow(&self) -> bool {
        false
    }
    unsafe fn feed(&self, _: &MainThread, p: *const u8, size: c_uint, pts: i64, es_data: c_int) -> c_char {
        sf_feed(p, size, pts, es_data)
    }
    unsafe fn unload(&self, _: &MainThread) {
        sf_unload()
    }
    unsafe fn callback_gate_retire(&self, _: &MainThread) -> c_int {
        CALLBACK_GATE_RETIRED.store(true, Relaxed);
        c_int::from(OBJECT_READY.load(Relaxed) && !LIFECYCLE_BLOCKED.load(Relaxed) && CALLBACK_INTERCEPTS.load(Relaxed) != 0)
    }
    unsafe fn callback_intercepts(&self, _: &MainThread) -> u32 {
        CALLBACK_INTERCEPTS.load(Relaxed)
    }
    unsafe fn destroy(&self, _: &MainThread) -> c_int {
        sf_destroy()
    }
    unsafe fn quarantine(&self, _: &MainThread) {
        sf_quarantine()
    }
    fn window_mode(&self) -> c_int {
        plx_platform::tv::sink::VP_EXPORTED
    }
    fn window_id(&self) -> &'static CStr {
        c"gst-window"
    }
    unsafe fn create_window(&self, _: &MainThread) -> *const c_char {
        c"gst-window".as_ptr()
    }
    #[allow(clippy::too_many_arguments)]
    unsafe fn place_window(&self, _: &MainThread, _src_w: c_int, _src_h: c_int, dst_x: c_int, dst_y: c_int, dst_w: c_int, dst_h: c_int) -> c_int {
        *DST.lock().unwrap_or_else(|e| e.into_inner()) = [dst_x, dst_y, dst_w, dst_h];
        1
    }
    unsafe fn destroy_window(&self, _: &MainThread) {}
    // ACB is the webOS 4 video plane; `VP_EXPORTED` never reaches it.
    unsafe fn plane_create(&self, _: &MainThread, _app_id: *const c_char, _player_type: c_int) -> c_long {
        0
    }
    unsafe fn plane_bind(&self, _: &MainThread, _media_id: *const c_char) {}
    unsafe fn plane_send_video_data(&self, _: &MainThread, _source_info: *const c_char) -> c_int {
        -1
    }
    unsafe fn plane_send_atmos(&self, _: &MainThread, _media_id: *const c_char) -> c_int {
        0
    }
    unsafe fn plane_start(&self, _: &MainThread, _x: c_long, _y: c_long, _w: c_long, _h: c_long) {}
    unsafe fn plane_unload(&self, _: &MainThread) {}
    unsafe fn plane_pause(&self, _: &MainThread) {}
    unsafe fn plane_resume(&self, _: &MainThread) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_codecs_select_their_branches() {
        let p = r#"{"option":{"externalStreamingInfo":{"codec":{"video":"H265","audio":"AC3 PLUS"}}}}"#;
        assert_eq!(payload_str(p, "video"), Some("H265"));
        assert_eq!(payload_str(p, "audio"), Some("AC3 PLUS"));
        assert_eq!(video_branch("H265").unwrap().1, "h265parse");
        assert_eq!(audio_branch("AC3 PLUS").unwrap().0, "audio/x-eac3,framed=true");
        assert_eq!(audio_branch("AAC").unwrap().1, "aacparse");
        assert!(video_branch("MPEG2").is_none());
        assert!(audio_branch("TRUEHD").is_none());
    }

    #[test]
    fn the_picture_is_letterboxed_into_its_rectangle() {
        assert_eq!(letterbox(1920, 1080, [0, 0, 1920, 1080]), (0.0, 0.0, 1920.0, 1080.0));
        // 4:3 pillarboxed on a 16:9 screen.
        assert_eq!(letterbox(1440, 1080, [0, 0, 1920, 1080]), (240.0, 0.0, 1440.0, 1080.0));
        // Scope 2.39:1 letterboxed.
        let (x, y, w, h) = letterbox(1920, 804, [0, 0, 1920, 1080]);
        assert_eq!((x, w), (0.0, 1920.0));
        assert!((y - 138.0).abs() < 0.01 && (h - 804.0).abs() < 0.01);
    }

    #[test]
    fn a_video_only_payload_builds_no_audio_lane() {
        let d = describe(video_branch("H264").unwrap(), None, "fakesink");
        assert!(d.contains("h264parse") && !d.contains("name=a"));
        let d = describe(video_branch("H264").unwrap(), audio_branch("AC3"), "fakesink");
        assert!(d.contains("name=a") && d.contains("ac3parse ! decodebin") && d.ends_with("fakesink"));
    }
}
