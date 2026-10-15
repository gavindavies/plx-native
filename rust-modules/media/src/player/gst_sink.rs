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
//! * audio → decoded to PCM (LG's four codecs by name; FLAC, Opus, Vorbis, TrueHD, MP3/MP2 and PCM
//!   from the parameters the demuxer reports, [`note_audio`]) → `audioconvert ! audioresample` →
//!   the system audio sink
//!   (`autoaudiosink`, or `PITV_GST_AUDIOSINK`), which also provides the clock both lanes
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
use super::sink::VideoSink;
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
    /// The audio lane is [`AudioLane::Described`] and its caps are still to be set.
    audio_undescribed: AtomicBool,
    /// End-of-stream was signalled while the audio lane may still have frames to come; the
    /// ticker ends it once its feed has run dry ([`end_drained_audio`]).
    audio_ending: AtomicBool,
    /// The last audio frame fed: its timestamp (fed timeline) and when, in [`now_ms`].
    audio_last_pts: AtomicI64,
    audio_fed_at_ms: AtomicU64,
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

/// Rank FFmpeg's DTS decoder above every other. GStreamer ranks libdca's `dtsdec` PRIMARY and
/// gst-libav's `avdec_dca` MARGINAL, but `dtsdec` decodes the DTS CORE only: a DTS-HD Master Audio
/// track would play lossy and at most 5.1. `avdec_dca` decodes the XLL extension, so the track
/// plays lossless at its own channel count. This process's registry only; nothing is persisted.
fn prefer_lossless_dts() {
    match gst::ElementFactory::find("avdec_dca") {
        Some(factory) => {
            factory.set_rank(gst::Rank::PRIMARY + 1);
            log("DTS: avdec_dca preferred (decodes DTS-HD MA losslessly)");
        }
        None => log("DTS: avdec_dca missing (install gstreamer1.0-libav); DTS-HD plays as its core"),
    }
}

/// The system GStreamer's version (`1.24.2`), as Settings › About names it. `None` when GStreamer
/// could not initialise.
pub fn gst_version() -> Option<String> {
    gst_ready().then(|| {
        let (major, minor, micro, _) = gst::version();
        format!("{major}.{minor}.{micro}")
    })
}

pub(crate) fn gst_ready() -> bool {
    static INIT: OnceLock<bool> = OnceLock::new();
    *INIT.get_or_init(|| match gst::init() {
        Ok(()) => {
            let (major, minor, micro, _) = gst::version();
            log(&format!("GStreamer {major}.{minor}.{micro} initialised"));
            prefer_lossless_dts();
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

/// Whether this machine's GStreamer has a decoder for the payload's video codec — the Linux
/// build's answer to the television's codec table (`devcaps`), which describes a set, not this
/// machine. Only the media type is asked: the parser in front of the decoder settles the rest.
pub fn decodes_video(codec: &str) -> bool {
    let Some((caps, _)) = video_branch(codec) else { return false };
    if !gst_ready() {
        return false;
    }
    let media_type = caps.split(',').next().unwrap_or(caps);
    let caps = gst::Caps::new_empty_simple(media_type);
    gst::ElementFactory::factories_with_type(gst::ElementFactoryType::DECODER, gst::Rank::MARGINAL)
        .iter()
        .any(|f| f.can_sink_any_caps(&caps))
}

/// How the audio lane's `appsrc` is described.
#[derive(Debug, Clone, Copy, PartialEq)]
enum AudioLane {
    /// LG's own vocabulary (`engine::audio_payload_codec`): the codec name is the whole
    /// description, with the codec's parser in front of `decodebin`. The demuxer frames AAC as
    /// ADTS for exactly this reason.
    Named { caps: &'static str, parser: &'static str },
    /// A codec the name alone does not describe (FLAC's STREAMINFO, Opus's and Vorbis's headers,
    /// PCM's sample format): the caps are built from the stream's parameters
    /// ([`note_audio`]) when its first frame is fed, and `decodebin` plugs the parser.
    Described,
}

/// The audio lane for the payload's audio codec.
fn audio_branch(codec: &str) -> Option<AudioLane> {
    let named = |caps, parser| AudioLane::Named { caps, parser };
    Some(match codec {
        "AC3" => named("audio/x-ac3,framed=true", "ac3parse"),
        "AC3 PLUS" => named("audio/x-eac3,framed=true", "ac3parse"),
        "AAC" => named("audio/mpeg,mpegversion=4,stream-format=adts", "aacparse"),
        "DTS" => named("audio/x-dts,framed=true", "dcaparse"),
        c if crate::player::engine::GST_AUDIO_PAYLOAD_CODECS.iter().any(|g| g.1 == c) => AudioLane::Described,
        _ => return None,
    })
}

// ---- described audio ----------------------------------------------------------------------------

/// What the demuxer knows about the audio stream it feeds: FFmpeg's codec name, the codec's
/// extradata (FLAC's metadata, the Opus head, Vorbis's three headers), and the PCM shape.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioParams {
    pub codec: String,
    pub extradata: Vec<u8>,
    pub rate: i32,
    pub channels: i32,
}

/// The audio stream the demuxer opened last. A Load reads it when its first audio frame arrives,
/// which is always after the demuxer chose that stream; a seek's fresh Load reads the same one.
static AUDIO_PARAMS: Mutex<Option<AudioParams>> = Mutex::new(None);

/// The demuxer's report of the audio stream it is about to feed (`ff::demux`).
pub fn note_audio(params: AudioParams) {
    *AUDIO_PARAMS.lock().unwrap_or_else(|e| e.into_inner()) = Some(params);
}

/// GStreamer's raw-audio format for an FFmpeg PCM codec whose packets are plain interleaved
/// samples. The framed variants (`pcm_bluray`, `pcm_dvd`) carry headers of their own and are not.
pub fn pcm_format(codec: &str) -> Option<&'static str> {
    Some(match codec {
        "pcm_u8" => "U8",
        "pcm_s8" => "S8",
        "pcm_s16le" => "S16LE",
        "pcm_s16be" => "S16BE",
        "pcm_s24le" => "S24LE",
        "pcm_s24be" => "S24BE",
        "pcm_s32le" => "S32LE",
        "pcm_s32be" => "S32BE",
        "pcm_f32le" => "F32LE",
        "pcm_f32be" => "F32BE",
        "pcm_f64le" => "F64LE",
        "pcm_f64be" => "F64BE",
        _ => return None,
    })
}

/// GStreamer's channel mask for FFmpeg's default layout of `channels`. Both enumerate the
/// positions used here in the same relative order (FL FR FC LFE, back, back-centre, side), so
/// the interleaving FFmpeg delivers is the order the mask names.
fn channel_mask(channels: i32) -> Option<u64> {
    const FL: u64 = 1 << 0;
    const FR: u64 = 1 << 1;
    const FC: u64 = 1 << 2;
    const LFE: u64 = 1 << 3;
    const RL: u64 = 1 << 4;
    const RR: u64 = 1 << 5;
    const RC: u64 = 1 << 8;
    const SL: u64 = 1 << 10;
    const SR: u64 = 1 << 11;
    Some(match channels {
        1 => 0,
        2 => FL | FR,
        3 => FL | FR | FC,
        4 => FL | FR | RL | RR,
        5 => FL | FR | FC | SL | SR,
        6 => FL | FR | FC | LFE | SL | SR,
        7 => FL | FR | FC | LFE | RC | SL | SR,
        8 => FL | FR | FC | LFE | RL | RR | SL | SR,
        _ => return None,
    })
}

/// FLAC's stream header as GStreamer's FLAC parser takes it in caps: the `fLaC` marker, then
/// each metadata block. FFmpeg's extradata is either the bare 34-byte STREAMINFO or the whole
/// `fLaC`-prefixed header.
fn flac_headers(extradata: &[u8]) -> Option<Vec<Vec<u8>>> {
    const STREAMINFO_LEN: usize = 34;
    let mut out = vec![b"fLaC".to_vec()];
    if let Some(mut rest) = extradata.strip_prefix(b"fLaC") {
        while rest.len() >= 4 {
            let len = usize::from(rest[1]) << 16 | usize::from(rest[2]) << 8 | usize::from(rest[3]);
            let block = rest.get(..4 + len)?;
            out.push(block.to_vec());
            rest = &rest[4 + len..];
            if block[0] & 0x80 != 0 {
                break;
            }
        }
    } else if extradata.len() == STREAMINFO_LEN {
        let mut block = vec![0x80, 0, 0, STREAMINFO_LEN as u8];
        block.extend_from_slice(extradata);
        out.push(block);
    }
    (out.len() > 1 && out[1][0] & 0x7f == 0).then_some(out)
}

/// The packets of a Xiph-laced extradata (Vorbis's identification, comment and setup headers):
/// a count less one, the lacing of every packet but the last, then the packets.
fn xiph_headers(extradata: &[u8]) -> Option<Vec<Vec<u8>>> {
    let (&count, mut rest) = extradata.split_first()?;
    let mut sizes = Vec::new();
    for _ in 0..count {
        let mut size = 0usize;
        loop {
            let (&b, r) = rest.split_first()?;
            rest = r;
            size += usize::from(b);
            if b != 255 {
                break;
            }
        }
        sizes.push(size);
    }
    let mut out = Vec::new();
    for size in sizes {
        out.push(rest.get(..size)?.to_vec());
        rest = &rest[size..];
    }
    if rest.is_empty() {
        return None;
    }
    out.push(rest.to_vec());
    Some(out)
}

/// An empty OpusTags packet: the decoder wants a comment header beside the head, and a Matroska
/// or MP4 Opus track carries only the head.
const OPUS_TAGS: &[u8] = b"OpusTags\0\0\0\0\0\0\0\0";

/// The caps a [`AudioLane::Described`] lane is given, from the demuxer's report.
fn described_caps(p: &AudioParams) -> Result<gst::Caps, String> {
    fn headers(caps: gst::caps::Builder<gst::caps::NoFeature>, packets: Vec<Vec<u8>>) -> gst::Caps {
        let array = gst::Array::new(packets.into_iter().map(|b| {
            let mut buf = gst::Buffer::from_mut_slice(b);
            buf.get_mut().map(|m| m.set_flags(gst::BufferFlags::HEADER));
            buf.to_send_value()
        }));
        caps.field("streamheader", array).build()
    }
    let bad = |what: &str| format!("{} stream has no usable {what}", p.codec);
    Ok(match p.codec.as_str() {
        "flac" => headers(
            gst::Caps::builder("audio/x-flac").field("framed", true),
            flac_headers(&p.extradata).ok_or_else(|| bad("STREAMINFO"))?,
        ),
        "vorbis" => {
            let packets = xiph_headers(&p.extradata).filter(|h| h.len() == 3).ok_or_else(|| bad("Vorbis headers"))?;
            headers(gst::Caps::builder("audio/x-vorbis"), packets)
        }
        "opus" => {
            let head = p.extradata.clone();
            if !head.starts_with(b"OpusHead") || head.len() < 19 {
                return Err(bad("OpusHead"));
            }
            let family = i32::from(head[18]);
            headers(gst::Caps::builder("audio/x-opus").field("channel-mapping-family", family), vec![head, OPUS_TAGS.to_vec()])
        }
        "mp3" => gst::Caps::builder("audio/mpeg").field("mpegversion", 1).field("layer", 3).build(),
        "mp2" => gst::Caps::builder("audio/mpeg").field("mpegversion", 1).field("layer", 2).build(),
        // The decoder negotiates its output from these before the first frame is parsed; without
        // them it never produces a sample and the pipeline never prerolls.
        "truehd" if p.rate > 0 && p.channels > 0 => {
            gst::Caps::builder("audio/x-true-hd").field("rate", p.rate).field("channels", p.channels).build()
        }
        "truehd" => return Err(bad("sample rate or channel count")),
        c => {
            let format = pcm_format(c).ok_or_else(|| format!("no GStreamer description for audio codec {c:?}"))?;
            let mask = channel_mask(p.channels).ok_or_else(|| bad("channel layout"))?;
            if p.rate <= 0 {
                return Err(bad("sample rate"));
            }
            gst::Caps::builder("audio/x-raw")
                .field("format", format)
                .field("layout", "interleaved")
                .field("rate", p.rate)
                .field("channels", p.channels)
                .field("channel-mask", gst::Bitmask::new(mask))
                .build()
        }
    })
}

/// The `gst-launch` description of the whole pipeline: the video lane always, the audio lane when
/// the payload declares one.
fn describe(video: (&str, &str), audio: Option<AudioLane>, audio_sink: &str) -> String {
    let mut d = format!(
        "appsrc name=v format=time is-live=false stream-type=stream block=false max-bytes={VIDEO_QUEUE_BYTES} caps=\"{}\" \
         ! {} ! decodebin ! videoconvert ! videoscale add-borders=true \
         ! video/x-raw,format=RGBA,pixel-aspect-ratio=1/1,width=[1,{MAX_W}],height=[1,{MAX_H}] \
         ! appsink name=vsink sync=true max-buffers=2 drop=false",
        video.0, video.1
    );
    let head = format!("appsrc name=a format=time is-live=false stream-type=stream block=false max-bytes={AUDIO_QUEUE_BYTES}");
    // `adec` is where the decoded format is read for the log (`log_audio_decoder`).
    let tail = format!("decodebin name=adecbin ! identity name=adec silent=true ! audioconvert ! audioresample ! {audio_sink}");
    match audio {
        Some(AudioLane::Named { caps, parser }) => d.push_str(&format!(" {head} caps=\"{caps}\" ! {parser} ! {tail}")),
        Some(AudioLane::Described) => d.push_str(&format!(" {head} ! {tail}")),
        None => {}
    }
    d
}

fn audio_sink_description() -> String {
    std::env::var("PITV_GST_AUDIOSINK")
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
        audio_undescribed: AtomicBool::new(audio == Some(AudioLane::Described)),
        audio_ending: AtomicBool::new(false),
        audio_last_pts: AtomicI64::new(i64::MIN),
        audio_fed_at_ms: AtomicU64::new(0),
        base: Arc::new(AtomicI64::new(i64::MIN)),
        generation: GENERATION.fetch_add(1, Relaxed) + 1,
    })
}

/// The session's own thread: the 5 Hz position report and the bus.
/// Say once which decoder GStreamer chose for the audio and what it produced — the format and
/// channel count that reach the speakers (a DTS-HD MA track decodes losslessly to its own channel
/// count, its core to at most 5.1). `true` once said, or when there is no audio lane.
fn log_audio_decoder(pipeline: &gst::Pipeline) -> bool {
    let Some(identity) = pipeline.by_name("adec") else { return true };
    let Some(caps) = identity.static_pad("src").and_then(|p| p.current_caps()) else { return false };
    let decoder = pipeline
        .by_name("adecbin")
        .and_then(|b| b.downcast::<gst::Bin>().ok())
        .and_then(|bin| {
            bin.iterate_recurse().into_iter().flatten().find_map(|e| {
                let factory = e.factory()?;
                factory.metadata("klass").is_some_and(|k| k.contains("Decoder") && k.contains("Audio")).then(|| factory.name().to_string())
            })
        })
        .unwrap_or_else(|| "none (raw PCM)".to_owned());
    log(&format!("audio decoded by {decoder}: {caps}"));
    true
}

fn start_ticker(pipeline: gst::Pipeline, base: Arc<AtomicI64>, generation: u64) {
    let spawned = std::thread::Builder::new().name("gstsink".into()).spawn(move || {
        let bus = match pipeline.bus() {
            Some(b) => b,
            None => return,
        };
        let mut audio_logged = false;
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
            let position = pipeline.query_position::<gst::ClockTime>().map(|pos| b.saturating_add(pos.nseconds() as i64));
            if let Some(p) = position {
                callback(0, p, c"");
            }
            if !audio_logged {
                audio_logged = log_audio_decoder(&pipeline);
            }
            end_drained_audio(generation, position);
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
    if es_data == 2 && s.audio_undescribed.swap(false, Relaxed) {
        let params = AUDIO_PARAMS.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match params.ok_or_else(|| "the demuxer reported no audio stream".to_owned()).and_then(|p| described_caps(&p)) {
            Ok(caps) => {
                // The media type only: a stream header in the caps runs to kilobytes.
                log(&format!("audio caps {}", caps.structure(0).map_or("?", |st| st.name().as_str())));
                src.set_caps(Some(&caps));
            }
            Err(e) => {
                log(&format!("audio refused: {e}"));
                return FEED_ERROR;
            }
        }
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
    if es_data == 2 {
        s.audio_last_pts.store(pts, Relaxed);
        s.audio_fed_at_ms.store(now_ms(), Relaxed);
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

/// The engine signals end-of-stream when the VIDEO lane drains (Kodi's convention, and what LG's
/// pipeline expects), and goes on feeding whatever audio is still queued behind it. So only the
/// video lane ends here; the audio lane ends when [`end_drained_audio`] sees its feed run dry.
unsafe fn sf_push_eos() -> c_int {
    with_session(|s| {
        let _ = s.video.end_of_stream();
        if s.audio.is_some() {
            s.audio_ending.store(true, Relaxed);
        }
        1
    })
    .unwrap_or(0)
}

/// Milliseconds on a monotonic clock private to this module.
fn now_ms() -> u64 {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// How long the audio feed must be quiet before an ending audio lane is ended.
const AUDIO_DRY_MS: u64 = 500;
/// How close the playhead must be to the last audio fed. The engine throttles a lane that is far
/// ahead of the playhead, so a quiet feed alone can be a pause; a feed that is quiet with the
/// playhead this close to its end is a lane with nothing left (the throttle would be feeding it).
const AUDIO_DRY_LEAD_NS: i64 = 1_000_000_000;

/// End the audio lane of an ending session once its feed has run dry: quiet for
/// [`AUDIO_DRY_MS`] with `position` (fed timeline) within [`AUDIO_DRY_LEAD_NS`] of its last frame.
fn end_drained_audio(generation: u64, position: Option<i64>) {
    with_session(|s| {
        let Some(a) = &s.audio else { return };
        if s.generation != generation || !s.audio_ending.load(Relaxed) {
            return;
        }
        let quiet = now_ms().saturating_sub(s.audio_fed_at_ms.load(Relaxed)) >= AUDIO_DRY_MS;
        let last = s.audio_last_pts.load(Relaxed);
        let caught_up = last == i64::MIN || position.is_some_and(|p| p.saturating_add(AUDIO_DRY_LEAD_NS) >= last);
        if quiet && caught_up && s.audio_ending.swap(false, Relaxed) {
            let _ = a.end_of_stream();
        }
    });
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
        super::sink::VP_EXPORTED
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
        assert_eq!(audio_branch("AC3 PLUS"), Some(AudioLane::Named { caps: "audio/x-eac3,framed=true", parser: "ac3parse" }));
        assert_eq!(audio_branch("AAC"), Some(AudioLane::Named { caps: "audio/mpeg,mpegversion=4,stream-format=adts", parser: "aacparse" }));
        assert_eq!(audio_branch("TRUEHD"), Some(AudioLane::Described));
        assert_eq!(audio_branch("FLAC"), Some(AudioLane::Described));
        assert!(video_branch("MPEG2").is_none());
        assert!(audio_branch("WMA").is_none());
    }

    #[test]
    fn a_described_audio_lane_leaves_its_caps_and_parser_to_the_feed() {
        let d = describe(video_branch("H264").unwrap(), audio_branch("OPUS"), "fakesink");
        let lane = &d[d.find("appsrc name=a").unwrap()..];
        assert!(!lane.contains("caps=") && lane.contains("! decodebin name=adecbin ! identity name=adec"), "{lane}");
    }

    #[test]
    fn flac_metadata_becomes_a_marker_and_its_blocks() {
        let info = [7u8; 34];
        let bare = flac_headers(&info).unwrap();
        assert_eq!(bare[0], b"fLaC");
        assert_eq!(bare[1][..4], [0x80, 0, 0, 34]);
        assert_eq!(bare[1].len(), 38);
        // The whole header: STREAMINFO (not last), then a 2-byte padding block flagged last.
        let mut full = b"fLaC".to_vec();
        full.extend_from_slice(&[0x00, 0, 0, 34]);
        full.extend_from_slice(&info);
        full.extend_from_slice(&[0x81, 0, 0, 2, 0, 0]);
        let h = flac_headers(&full).unwrap();
        assert_eq!(h.len(), 3);
        assert_eq!(h[2], [0x81, 0, 0, 2, 0, 0]);
        assert!(flac_headers(&info[..20]).is_none());
        assert!(flac_headers(b"fLaC\x00\x00\x00\x22").is_none(), "a block cut short");
    }

    #[test]
    fn xiph_lacing_splits_the_three_vorbis_headers() {
        let mut ed = vec![2u8, 255, 3, 4];
        ed.extend(std::iter::repeat_n(1u8, 258));
        ed.extend([2u8; 4]);
        ed.extend([3u8; 5]);
        let h = xiph_headers(&ed).unwrap();
        assert_eq!(h.iter().map(Vec::len).collect::<Vec<_>>(), [258, 4, 5]);
        assert_eq!(h[2], [3u8; 5]);
        assert!(xiph_headers(&[2, 10, 10, 1, 2]).is_none(), "lacing longer than the data");
    }

    #[test]
    fn pcm_is_described_only_when_its_packets_are_plain_samples() {
        assert_eq!(pcm_format("pcm_s24le"), Some("S24LE"));
        assert_eq!(pcm_format("pcm_bluray"), None);
        assert_eq!(channel_mask(2), Some(0b11));
        assert_eq!(channel_mask(6), Some(0b1100_0000_1111));
        assert_eq!(channel_mask(9), None);
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
