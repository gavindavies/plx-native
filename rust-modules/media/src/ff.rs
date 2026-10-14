//! FFmpeg FFI (libavformat / libavcodec / libavutil). The TV ships FFmpeg n3.3
//! (SONAMEs libavformat.so.57 / libavcodec.so.57 / libavutil.so.55 = 57.71.100 /
//! 57.89.100 / 55.58.100); we link stub `.so`s carrying those SONAMEs and the device
//! loads the real libraries at runtime — the same stub trick as SDL/GLES/Starfish.
//!
//! This module is THE media demuxer: robust MKV/MP4/TS demux, HTTP input, and
//! index-based seeking (av_seek_frame). Design record: docs/ffmpeg-demuxer-plan.md.
//! It also owns the ONE encode path (`venc`, near the end of the file): the dev
//! capture stream's MPEG1 + MPEG-TS muxer, kept here because every FFmpeg ABI
//! detail — struct offsets, AVOption names, custom AVIO — belongs in one module.
//! The struct layouts below are the FFmpeg n3.3 ABI, confirmed on-device by the
//! Phase A probe (/tmp/pitv-ffprobe logs codec_id/width/height for a known title).
use crate::aq::AuQueue;
use crate::player::threads::SendPtr;
use crate::player::SHARED;
use std::os::raw::{c_char, c_int, c_long, c_uint, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

/// Feed audio (es=2) to the pipeline. Cleared by the /tmp/pitv-noaudio dev trigger to
/// A/B whether the audio ES (E-AC3/Atmos) is what stalls the sink on 4K HEVC.
static FEED_AUDIO: AtomicBool = AtomicBool::new(true);
/// Does the FFmpeg on this device have the ABI these offsets were written for? Set once by
/// [`boot`]; read by [`demux`]. Starts FALSE so the gate is closed until boot has actually looked
/// — a demux that somehow ran first should refuse, not assume.
static ABI_OK: AtomicBool = AtomicBool::new(false);
/// Did this demux session hand a single video AU to a lane? Cleared by `demux` on entry.
///
/// The tail uses it to tell "ended cleanly" from "ended having produced nothing", which EOF alone
/// cannot express — and which is the difference between a real end-of-file and the player sitting
/// on "Buffering…" forever with no error.
static PUSHED_ANY: AtomicBool = AtomicBool::new(false);
/// Did libswscale load? It is the one FFmpeg library that is NOT required: `RELEASE=1` leaves it
/// out of the package because only the dev capture stream's RGBA->YUV conversion uses it. `venc`
/// checks this rather than relying on being feature-gated out, so the dependency is enforced
/// where it exists instead of somewhere else.
static SWS_OK: AtomicBool = AtomicBool::new(false);
pub fn set_feed_audio(on: bool) {
    FEED_AUDIO.store(on, Ordering::Relaxed);
}

// ---- opaque handles (pointer-only, never dereferenced) ----
pub enum AVClass {}
pub enum AVInputFormat {}
pub enum AVIOContext {}
pub enum AVDictionary {}
pub enum AVCodec {}
pub enum AVCodecContext {} // opaque: we only pass the lib-allocated pointer to decode/free
pub enum AVBitStreamFilter {}
pub enum AVBufferRef {}
// AVStream is opaque here: it is 712 bytes with a large "internal but ABI" block, so
// rather than transcribe every field we read only the three we need at their verified
// n3.3 offsets (index +0, time_base +40, codecpar +708). Phase A confirms the offsets.
pub enum AVStream {}
// ---- The FFmpeg 9.0 ABI, on 32-bit ARM. ----
//
// These were a RUNTIME TABLE selected per libavformat major, because the app read the
// television's own FFmpeg and every webOS release ships a different one (55 / 57 / 58 / 59 / 60
// across webOS 2 to 11). They are plain constants again because the app now ships its own
// FFmpeg: exactly one version exists, under a SONAME that cannot collide with the TV's, and
// `ci/ffabi-assert.c` holds every number here against the very headers the shipped libraries
// were built from.
//
// That is the point of bundling. An offset that is merely *probably* right — derived from a
// version string plus an upstream tarball — becomes one the compiler checked.
//
// `tools/ffabi-dump.sh` re-derives all of them if the bundled version ever changes.
//
// **TWO TABLES, and the axis is POINTER WIDTH rather than FFmpeg version.** The 32-bit arm is the
// television; the 64-bit arm is the desktop simulator (`make sim`), which runs this same code
// against a host build of the SAME FFmpeg 9.0 from the SAME `ci/build-ffmpeg.sh` component list.
// Every difference below is a pointer that got wider or an int64 that moved to keep its
// alignment; nothing here is a version difference, which is exactly why the old runtime
// major-selected table does not come back. `ci/ffabi-assert.c` holds both, `#if`-ed the same way,
// against each build's own headers — so a wrong number is a compile error on the platform it is
// wrong for, and `HOST=1 tools/ffabi-dump.sh` prints the 64-bit half.
#[cfg(target_pointer_width = "32")]
const OFF_STREAM_INDEX: usize = 4; // NB not 0 — FFmpeg 5.0 put `const AVClass *av_class` first
#[cfg(target_pointer_width = "32")]
const OFF_STREAM_CODECPAR: usize = 12;
#[cfg(target_pointer_width = "32")]
const OFF_STREAM_TIME_BASE: usize = 20;
#[cfg(target_pointer_width = "64")]
const OFF_STREAM_CODECPAR: usize = 16;
#[cfg(target_pointer_width = "64")]
const OFF_STREAM_TIME_BASE: usize = 32;
/// `AVStream.metadata` — the container's own per-track tags, which is where a track's NAME lives.
///
/// It is read for one reason and it is a data reason, not a rendering one: **PMS does not send the
/// per-track title for an MP4.** Verified live 2026-08-22 against one server holding both — for a
/// Matroska part PMS sends `Stream.title` (`"HDRezka Studio"`, `"Forced"`, `"SDH"`) and the track
/// menu has always drawn it; for an MP4 part it sends no `title` key at all, though the file
/// carries a `name` tag on every track (`"Полные Jaskier"`, `"Форс. iTunes"`, `"Full SDH"`).
/// Matroska spells the tag `title` and MP4 spells it `name`, and Plex's parser maps only the first.
/// So a nine-track MP4 arrives as six rows reading `Русский` and nothing else — the user cannot
/// tell a forced signs track from a full translation, and no amount of care in the UI can invent
/// the difference. `/library/streams/{id}` is 501 and `checkFiles=1` adds nothing; the file is the
/// only source, and we are already holding it open.
#[cfg(target_pointer_width = "32")]
const OFF_STREAM_METADATA: usize = 72;
#[cfg(target_pointer_width = "64")]
const OFF_STREAM_METADATA: usize = 80;
/// `AVStream.disposition` (the `AV_DISPOSITION_*` flags), after the 8-aligned `nb_frames`: which
/// track a local file marks default, forced or for the hard of hearing (`probe_local`).
#[cfg(target_pointer_width = "32")]
const OFF_STREAM_DISPOSITION: usize = 56;
#[cfg(target_pointer_width = "64")]
const OFF_STREAM_DISPOSITION: usize = 64;
const AV_DISPOSITION_DEFAULT: c_int = 0x0001;
const AV_DISPOSITION_FORCED: c_int = 0x0040;
const AV_DISPOSITION_HEARING_IMPAIRED: c_int = 0x0080;
/// `AVStream.avg_frame_rate`, directly after `metadata` — the frame rate a local file declares when
/// its demuxer leaves `codecpar.framerate` unset (`probe_local`).
#[cfg(target_pointer_width = "32")]
const OFF_STREAM_AVG_FRAME_RATE: usize = 76;
#[cfg(target_pointer_width = "64")]
const OFF_STREAM_AVG_FRAME_RATE: usize = 88;
/// `AVFormatContext.duration`, in AV_TIME_BASE units. By offset — see the struct's closing note.
#[cfg(target_pointer_width = "32")]
const OFF_FMT_DURATION: usize = 64;
#[cfg(target_pointer_width = "64")]
const OFF_FMT_DURATION: usize = 104;

/// `AVDictionaryEntry` — two `char *`. Modelled rather than opaque because the whole point of the
/// call is to read both halves; `ci/ffabi-assert.c` holds the layout.
#[repr(C)]
pub struct AVDictionaryEntry {
    key: *const c_char,
    value: *const c_char,
}

/// The only field this app reads past `AVFormatContext.streams`.
#[inline]
unsafe fn fmt_duration(fmt: *const AVFormatContext) -> i64 {
    *((fmt as *const u8).add(OFF_FMT_DURATION) as *const i64)
}

// ---- structs (exact n3.3 field order; 32-bit ARM/AAPCS sizes) ----
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AVRational {
    pub num: c_int,
    pub den: c_int,
}

// sizeof = 72
#[repr(C)]
pub struct AVPacket {
    pub buf: *mut AVBufferRef,            // +0
    pub pts: i64,                         // +8
    pub dts: i64,                         // +16
    pub data: *mut u8,                    // +24
    pub size: c_int,                      // +28
    pub stream_index: c_int,              // +32
    pub flags: c_int,                     // +36
    pub side_data: *mut AVPacketSideData, // +40
    pub side_data_elems: c_int,           // +44
    pub duration: i64,                    // +48
    pub pos: i64,                         // +56
    // The tail changed at FFmpeg 5.0: `convergence_duration` went with
    // FF_API_CONVERGENCE_DURATION, and these three arrived. sizeof 72 -> 80. The app never
    // allocates an AVPacket (av_packet_alloc does), so only the offsets above matter — but the
    // size is asserted anyway, because a short model here would be a silent overread.
    pub opaque: *mut c_void,          // +64
    pub opaque_ref: *mut AVBufferRef, // +68
    pub time_base: AVRational,        // +72
}

/// `AVPacketSideData` — one entry of `AVCodecParameters::coded_side_data`. It was an opaque
/// `enum` here for as long as nothing read it; the Dolby Vision configuration record is the first
/// thing that does.
///
/// sizeof = 12 on 32-bit ARM, proven with `ci/ffabi-assert.c`. `size` is a `size_t`, so `usize`
/// is the field's type on both the target and the host rather than a number that happens to match
/// — the one place in this table where the Rust type carries the ABI instead of a comment.
#[repr(C)]
pub struct AVPacketSideData {
    pub data: *mut u8, // +0
    pub size: usize,   // +4
    pub type_: c_int,  // +8  (enum AVPacketSideDataType)
}

/// `AVDOVIDecoderConfigurationRecord` (libavutil/dovi_meta.h), the payload of an
/// `AV_PKT_DATA_DOVI_CONF` side-data entry — what the mp4 `dvcC`/`dvvC` box and the Matroska
/// `DolbyVisionConfiguration` block carry, handed over by BOTH bundled demuxers.
///
/// **Nine plain `uint8_t`, so this is the rare zero-risk read in this file**: no integers wider
/// than a byte, no pointers, nothing to align and nothing to pad, on any target. Field order and
/// count verified against the vendored FFmpeg 9.0 header, and each offset proven for 32-bit ARM
/// in `ci/ffabi-assert.c` — which is cheap insurance rather than doubt, since the assertions cost
/// nothing and the struct is public ABI that could gain a field at any major.
///
/// The upstream spelling is `AVDOVI…`, not `AVDovi…`, and the difference matters only to anyone
/// grepping the header for it.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct AVDOVIDecoderConfigurationRecord {
    pub dv_version_major: u8,              // +0
    pub dv_version_minor: u8,              // +1
    pub dv_profile: u8,                    // +2
    pub dv_level: u8,                      // +3
    pub rpu_present_flag: u8,              // +4
    pub el_present_flag: u8,               // +5
    pub bl_present_flag: u8,               // +6
    pub dv_bl_signal_compatibility_id: u8, // +7
    pub dv_md_compression: u8,             // +8
}

// sizeof = 136
#[repr(C)]
pub struct AVCodecParameters {
    pub codec_type: c_int,                      // +0
    pub codec_id: c_int,                        // +4
    pub codec_tag: u32,                         // +8
    pub extradata: *mut u8,                     // +12
    pub extradata_size: c_int,                  // +16
    pub coded_side_data: *mut AVPacketSideData, // +20
    pub nb_coded_side_data: c_int,              // +24
    pub format: c_int,                          // +28
    pub bit_rate: i64,                          // +32
    pub bits_per_coded_sample: c_int,           // +40
    pub bits_per_raw_sample: c_int,             // +44
    pub profile: c_int,                         // +48
    pub level: c_int,                           // +52
    pub width: c_int,                           // +56
    pub height: c_int,                          // +60
    pub sample_aspect_ratio: AVRational,        // +64
    pub framerate: AVRational,                  // +72
    pub field_order: c_int,                     // +80
    pub color_range: c_int,                     // +84
    pub color_primaries: c_int,                 // +88
    pub color_trc: c_int,                       // +92
    pub color_space: c_int,                     // +96
    pub chroma_location: c_int,                 // +100
    pub video_delay: c_int,                     // +104
    // FFmpeg 7 DELETED the deprecated `channel_layout`/`channels` pair that lived here and left
    // only this struct. `nb_channels` is the replacement for the `channels` int, and reading the
    // old field is not a compile error on a hand-written model — it is a silent read of whatever
    // now occupies +104. Which is the argument for shipping a known FFmpeg in one sentence.
    pub ch_layout: AVChannelLayout, // +112
    pub sample_rate: c_int,         // +136
    pub block_align: c_int,         // +140
    pub frame_size: c_int,          // +144
    pub initial_padding: c_int,     // +148
    pub trailing_padding: c_int,    // +152
    pub seek_preroll: c_int,        // +156
    pub alpha_mode: c_int,          // +160
}

/// `AVChannelLayout`, modelled only as far as `nb_channels` — the one field this app reads.
/// The union that follows is 8-aligned, so a 4-byte pad sits after `nb_channels` on ARM.
#[repr(C)]
pub struct AVChannelLayout {
    pub order: c_int,        // +0
    pub nb_channels: c_int,  // +4
    pub u_mask: u64,         // +8  (union { mask, map })
    pub opaque: *mut c_void, // +16
}

// AVFormatContext truncated after `filename` — we only ever hold a library-returned pointer and
// read leading fields. NEVER stack-allocate this.
//
// It STOPS at `filename` because that is where the two majors stop agreeing: FFmpeg 4.0 inserted
// `char *url` at +1056, pushing start_time to +1064 and duration to +1072. Everything above is
// identical on 3.3 and 4.x. `duration` — the only field past that point this app reads — is
// reached through [`fmt_duration`] at the offset the runtime ABI table gives, rather than by
// declaring it here at an offset that is right on exactly one of the two.
#[repr(C)]
pub struct AVFormatContext {
    pub av_class: *const AVClass,    // +0
    pub iformat: *mut AVInputFormat, // +4
    pub oformat: *mut c_void,        // +8
    pub priv_data: *mut c_void,      // +12
    pub pb: *mut AVIOContext,        // +16
    pub ctx_flags: c_int,            // +20
    pub nb_streams: c_uint,          // +24
    pub streams: *mut *mut AVStream, // +28
                                     // STOPS HERE. Everything past `streams` shifts between majors — `duration` is at +48 on
                                     // FFmpeg 6, +64 on 9, and +1064 on the n3.3 the televisions ship, because 5.0 deleted
                                     // `filename[1024]` and later releases kept rearranging what replaced it. It is the only field
                                     // beyond this point the app reads, so it is read at OFF_FMT_DURATION rather than declared at
                                     // a position that would be quietly wrong after a version bump.
}

// AVBSFContext is NOT modelled. The app does its own AVCC->Annex-B conversion in Rust, so the
// bitstream-filter API is never driven — only `av_bsf_get_by_name` survives, and only to log
// whether the filters exist. Keeping a #[repr(C)] mirror of a struct nothing reads meant
// re-deriving its offsets at every FFmpeg bump to prove a fact that did not matter.

// AVSubtitle (avcodec.h) — one decoded subtitle "display set". sizeof 32 on 32-bit ARM
// (u16 format @0; u32 start/end_display_time @4/@8, ms relative to `pts`; num_rects @12;
// rects ptr @16; i64 pts @24 in AV_TIME_BASE µs). repr(C) inserts the +20 pad before pts.
#[repr(C)]
pub struct AVSubtitle {
    pub format: u16,                     // +0  (0 = graphics/bitmap)
    pub start_display_time: u32,         // +4
    pub end_display_time: u32,           // +8
    pub num_rects: c_uint,               // +12
    pub rects: *mut *mut AVSubtitleRect, // +16
    pub pts: i64,                        // +24
}

// AVSubtitleRect (avcodec.h, libavcodec 57 / FFmpeg 3.x). CRUCIAL ABI detail: with
// FF_API_AVPICTURE (major < 58, i.e. this build) a DEPRECATED `AVPicture pict` is embedded
// after nb_colors — 8 data ptrs + 8 linesizes (64 bytes) — which shifts the live `data[4]`
// /`linesize[4]` down by 64. The Pass-A probe logs both the `data[]` and the `pict_data[]`
// slots to confirm which the decoder populates before Pass B reads pixels. sizeof = 132.
#[repr(C)]
pub struct AVSubtitleRect {
    pub x: c_int,         // +0
    pub y: c_int,         // +4
    pub w: c_int,         // +8
    pub h: c_int,         // +12
    pub nb_colors: c_int, // +16
    // +20 — was +84 on the TV's FFmpeg 3.3, because an entire deprecated AVPicture sat ahead of
    // it under FF_API_AVPICTURE. FFmpeg 5.0 deleted that, taking sizeof from 132 to 68.
    pub data: [*mut u8; 4], // +20  data[0]=PAL8 indices, data[1]=palette (256×BGRA)
    pub linesize: [c_int; 4], // +36
    // **`flags` comes BEFORE `type`, and this model had them the other way round until
    // 2026-08-28.** The consequence was latent rather than live — `rect_to_indexed` reads only
    // x/y/w/h/linesize[0]/data[0..2], all of which are ahead of the swap — but it was wrong in
    // the way this whole apparatus exists to prevent: `type_` read `flags`, `text` read `type`,
    // and on the 64-bit host `flags` landed at offset 96 of a 96-byte struct, i.e. one word past
    // the end. Found by porting the table to the simulator's pointer width, which is a second
    // independent reading of the same header and is exactly what caught it. `ci/ffabi-assert.c`
    // now pins all four on both widths.
    pub flags: c_int,      // +52 arm / +72 host
    pub type_: c_int,      // +56 / +76 (enum AVSubtitleType: 0=NONE, 1=BITMAP, 2=TEXT, 3=ASS)
    pub text: *mut c_char, // +60 / +80
    pub ass: *mut c_char,  // +64 / +88
}

// ---- externs, bound at RUNTIME by SONAME candidate list (see `dynlib`) ----
//
// These were four `#[link]` directives, which put `libavformat.so.57` and its three siblings in
// DT_NEEDED. That is fatal on any firmware that moved the major: webOS 5.3.1 ships 58/58/56/5,
// 10.2.0 ships 59/59/57/6, 11.2.0 ships 60/60/58/7 — and a missing DT_NEEDED kills the process at
// exec(), before main, before this log file is even open. DT_NEEDED also cannot say "57 OR 58", so
// one binary spanning both eras has to do the resolution itself.
//
// The candidate lists reach past the majors this table describes ON PURPOSE. Opening a
// libavformat 59 does not make the offsets below correct — `boot()` still refuses it — but the
// difference between refusing and never starting is the difference between a UI that browses your
// library and says it cannot play, and a television where nothing happens at all.
//
// One consequence worth knowing: the `cfg(test)` gate is gone. With no link directive the host
// suite links unconditionally, and a test that calls into FFmpeg now fails by taking `dlopen`'s
// None branch on Darwin instead of by failing to link.
plx_base::dynlib! {
    avformat: [
        // The ELF the app SHIPS, and the Mach-O the desktop simulator builds beside it
        // (`HOST=1 ci/build-ffmpeg.sh` — same version, same component list). A candidate
        // LIST rather than a `cfg`: `dynlib!` already tries each name and reports the ones
        // it could not open, and only one of these two can ever exist in an app directory.
        "libavformat-plx.so.63",
        "libavformat-plx.63.dylib",
    ] {
    // Declared beside libavformat's other entry points because that is the library that DEFINES
    // it. Under `#[link]` the final link resolved every name against every library at once and
    // the grouping was cosmetic; `dlsym` searches one handle and its dependency chain, and
    // libavutil does not depend on libavformat — so leaving this in the avutil block reported the
    // whole avutil table as Incomplete on every device, including working ones.
    fn avformat_version() -> c_uint;
    fn avformat_network_init() -> c_int;
    fn avformat_alloc_context() -> *mut AVFormatContext;
    fn avformat_open_input(
        ps: *mut *mut AVFormatContext,
        url: *const c_char,
        fmt: *mut AVInputFormat,
        options: *mut *mut AVDictionary,
    ) -> c_int;
    fn avformat_find_stream_info(ic: *mut AVFormatContext, options: *mut *mut AVDictionary) -> c_int;
    fn av_find_best_stream(
        ic: *mut AVFormatContext,
        type_: c_int,
        wanted: c_int,
        related: c_int,
        decoder_ret: *mut *mut AVCodec,
        flags: c_int,
    ) -> c_int;
    fn av_read_frame(s: *mut AVFormatContext, pkt: *mut AVPacket) -> c_int;
    fn av_seek_frame(s: *mut AVFormatContext, stream_index: c_int, ts: i64, flags: c_int) -> c_int;
    fn avformat_close_input(s: *mut *mut AVFormatContext);
    fn avio_alloc_context(
        buffer: *mut u8,
        buffer_size: c_int,
        write_flag: c_int,
        opaque: *mut c_void,
        read_packet: Option<extern "C" fn(*mut c_void, *mut u8, c_int) -> c_int>,
        write_packet: Option<extern "C" fn(*mut c_void, *mut u8, c_int) -> c_int>,
        seek: Option<extern "C" fn(*mut c_void, i64, c_int) -> i64>,
    ) -> *mut AVIOContext;
    // MPEG-TS mux (dev capture stream, venc section)
    fn avformat_alloc_output_context2(
        ctx: *mut *mut AVFormatContext,
        oformat: *mut c_void,
        format_name: *const c_char,
        filename: *const c_char,
    ) -> c_int;
    fn avformat_new_stream(s: *mut AVFormatContext, c: *const AVCodec) -> *mut AVStream;
    fn avformat_write_header(s: *mut AVFormatContext, options: *mut *mut AVDictionary) -> c_int;
    fn av_write_frame(s: *mut AVFormatContext, pkt: *mut AVPacket) -> c_int;
    fn avio_flush(s: *mut AVIOContext);
    fn avformat_free_context(s: *mut AVFormatContext);
}}
plx_base::dynlib! {
    avcodec: [
        // The ELF the app SHIPS, and the Mach-O the desktop simulator builds beside it
        // (`HOST=1 ci/build-ffmpeg.sh` — same version, same component list). A candidate
        // LIST rather than a `cfg`: `dynlib!` already tries each name and reports the ones
        // it could not open, and only one of these two can ever exist in an app directory.
        "libavcodec-plx.so.63",
        "libavcodec-plx.63.dylib",
    ] {
    fn avcodec_version() -> c_uint;
    fn av_packet_alloc() -> *mut AVPacket;
    fn av_packet_free(pkt: *mut *mut AVPacket);
    fn av_packet_unref(pkt: *mut AVPacket);
    fn avcodec_get_name(id: c_int) -> *const c_char;
    fn av_bsf_get_by_name(name: *const c_char) -> *const AVBitStreamFilter;
    // Image-subtitle software decode (PGS/VobSub/DVB → paletted bitmap). The classic 3.x
    // API: decode_subtitle2 fills an AVSubtitle the caller must avsubtitle_free.
    fn avcodec_find_decoder(id: c_int) -> *const AVCodec;
    fn avcodec_alloc_context3(codec: *const AVCodec) -> *mut AVCodecContext;
    fn avcodec_parameters_to_context(ctx: *mut AVCodecContext, par: *const AVCodecParameters) -> c_int;
    fn avcodec_open2(ctx: *mut AVCodecContext, codec: *const AVCodec, opts: *mut *mut AVDictionary) -> c_int;
    fn avcodec_decode_subtitle2(
        ctx: *mut AVCodecContext,
        sub: *mut AVSubtitle,
        got_sub: *mut c_int,
        pkt: *mut AVPacket,
    ) -> c_int;
    fn avsubtitle_free(sub: *mut AVSubtitle);
    fn avcodec_free_context(ctx: *mut *mut AVCodecContext);
    // Scratch AVCodecParameters for `sub_canvas` — library-allocated so its true size (136 on
    // this build) is never our problem, and `from_context` starts by av_freep-ing par->extradata.
    // Both PRESENT on the device's own libavcodec.so.57.89.100 (`tools/abi-probe.sh has
    // libavcodec.so.57 avcodec_parameters_alloc avcodec_parameters_free`, 2026-07-29) — the stub
    // .so links either way, so this is checked, not assumed.
    fn avcodec_parameters_alloc() -> *mut AVCodecParameters;
    fn avcodec_parameters_free(par: *mut *mut AVCodecParameters);
    fn avcodec_find_encoder_by_name(name: *const c_char) -> *const AVCodec;
    // MPEG1 encode (dev capture stream, venc section)
    fn avcodec_send_frame(ctx: *mut AVCodecContext, frame: *const c_void) -> c_int;
    fn avcodec_receive_packet(ctx: *mut AVCodecContext, pkt: *mut AVPacket) -> c_int;
    fn avcodec_parameters_from_context(par: *mut AVCodecParameters, ctx: *const AVCodecContext) -> c_int;
    fn av_packet_rescale_ts(pkt: *mut AVPacket, tb_src: AVRational, tb_dst: AVRational);
}}
plx_base::dynlib! {
    // avformat_version lives in libavformat, not libavutil — but it was declared here and the
    // loader resolves by symbol, not by header, so it must move to the library that defines it or
    // the whole avutil table reports Incomplete on every device.
    avutil: [
        // The ELF the app SHIPS, and the Mach-O the desktop simulator builds beside it
        // (`HOST=1 ci/build-ffmpeg.sh` — same version, same component list). A candidate
        // LIST rather than a `cfg`: `dynlib!` already tries each name and reports the ones
        // it could not open, and only one of these two can ever exist in an app directory.
        "libavutil-plx.so.61",
        "libavutil-plx.61.dylib",
    ] {
    fn avutil_version() -> c_uint;
    fn av_malloc(size: usize) -> *mut c_void;
    fn av_freep(ptr: *mut c_void);
    fn av_rescale_q(a: i64, bq: AVRational, cq: AVRational) -> i64;
    // MPEG1 encode input frames + option/pixfmt helpers (venc section)
    fn av_frame_alloc() -> *mut c_void;
    fn av_frame_free(frame: *mut *mut c_void);
    fn av_frame_get_buffer(frame: *mut c_void, align: c_int) -> c_int;
    fn av_frame_make_writable(frame: *mut c_void) -> c_int;
    fn av_opt_set(obj: *mut c_void, name: *const c_char, val: *const c_char, search_flags: c_int) -> c_int;
    fn av_get_pix_fmt(name: *const c_char) -> c_int;
    // Container tags. Borrowed, never freed: the entry belongs to the dictionary, which belongs to
    // the AVStream, which `avformat_close_input` frees — so every read must copy before the close.
    fn av_dict_get(
        m: *const AVDictionary,
        key: *const c_char,
        prev: *const AVDictionaryEntry,
        flags: c_int,
    ) -> *const AVDictionaryEntry;
}}
plx_base::dynlib! {
    swscale: [
        // The ELF the app SHIPS, and the Mach-O the desktop simulator builds beside it
        // (`HOST=1 ci/build-ffmpeg.sh` — same version, same component list). A candidate
        // LIST rather than a `cfg`: `dynlib!` already tries each name and reports the ones
        // it could not open, and only one of these two can ever exist in an app directory.
        "libswscale-plx.so.10",
        "libswscale-plx.10.dylib",
    ] {
    fn sws_getContext(
        src_w: c_int, src_h: c_int, src_fmt: c_int,
        dst_w: c_int, dst_h: c_int, dst_fmt: c_int,
        flags: c_int, src_filter: *mut c_void, dst_filter: *mut c_void, param: *const f64,
    ) -> *mut c_void;
    fn sws_scale(
        c: *mut c_void,
        src_slice: *const *const u8, src_stride: *const c_int,
        src_slice_y: c_int, src_slice_h: c_int,
        dst: *const *mut u8, dst_stride: *const c_int,
    ) -> c_int;
    fn sws_freeContext(c: *mut c_void);
}}

// ---- constants (n3.3) ----
pub const AVMEDIA_TYPE_VIDEO: c_int = 0;
pub const AVMEDIA_TYPE_AUDIO: c_int = 1;
pub const AVMEDIA_TYPE_SUBTITLE: c_int = 3;
// Proven against the bundled headers by ci/ffabi-assert.c.
const AVMEDIA_TYPE_ATTACHMENT: c_int = 4;
// The codec ids the app compares against are AAC, H264, HEVC, VP9 and AV1. Their values shift between
// FFmpeg majors, so each is asserted against the bundled headers in ci/ffabi-assert.c.
// AV1 needs no FFmpeg parser or bsf: matroska (V_AV1) and mov (av01) demux it without one, and
// with the av1 parser absent the packets arrive raw. `av1_packet_to_tu` frames the OBUs itself.
// Enabling the parser would pull in cbs_av1 for nothing.
pub const AV_CODEC_ID_AAC: c_int = 0x15002;
pub const AV_CODEC_ID_H264: c_int = 27;
// FF_API_XVMC and FF_API_VOXWARE both died before FFmpeg 6, which is why these differ from the
// values the n3.3 televisions use (28 / 174 / 0x15029).
pub const AV_CODEC_ID_HEVC: c_int = 172;
pub const AV_CODEC_ID_VP9: c_int = 166;
pub const AV_CODEC_ID_AV1: c_int = 222;
pub const AV_PKT_FLAG_KEY: c_int = 1;
pub const AVSEEK_FLAG_BACKWARD: c_int = 1;
pub const AVERROR_EOF: c_int = -541478725;
pub const AVERROR_EAGAIN: c_int = -11;
/// `AVERROR(EIO)`: unlike EOF, tells libavformat and our producer that the source was truncated by
/// a transport failure.
pub const AVERROR_IO: c_int = -5;
pub const AV_NOPTS_VALUE: i64 = i64::MIN;
pub const AV_TIME_BASE: i64 = 1_000_000;
pub const NS_TB: AVRational = AVRational {
    num: 1,
    den: 1_000_000_000,
};
/// `AV_PKT_DATA_DOVI_CONF` — the side-data type tag whose payload is an
/// [`AVDOVIDecoderConfigurationRecord`]. 29 in FFmpeg 9.0; asserted in `ci/ffabi-assert.c`
/// because `AVPacketSideDataType` is an ordinary sequential enum and every value in it shifts
/// when a member is inserted above.
pub const AV_PKT_DATA_DOVI_CONF: c_int = 29;
/// `AV_PKT_DATA_MASTERING_DISPLAY_METADATA` (an `AVMasteringDisplayMetadata`, 88 bytes) and
/// `AV_PKT_DATA_CONTENT_LIGHT_LEVEL` (an `AVContentLightMetadata`, 8 bytes): 20 and 22 in FFmpeg 9.0,
/// asserted in `ci/ffabi-assert.c` for the same reason as the Dolby Vision tag. The two structs'
/// member offsets are hard-coded in `player::vp9_hdr::{parse_mastering, parse_cll}` and pinned by
/// the same file; a bump that moves them has to change both.
pub const AV_PKT_DATA_MASTERING_DISPLAY_METADATA: c_int = 20;
pub const AV_PKT_DATA_CONTENT_LIGHT_LEVEL: c_int = 22;

// ---- AVStream field accessors, at the constants above. Read by offset rather than modelled:
// the struct is large, mostly internal, and only three fields are wanted. ----
#[inline]
unsafe fn stream_codecpar(s: *mut AVStream) -> *mut AVCodecParameters {
    *((s as *const u8).add(OFF_STREAM_CODECPAR) as *const *mut AVCodecParameters)
}
#[inline]
unsafe fn stream_time_base(s: *mut AVStream) -> AVRational {
    *((s as *const u8).add(OFF_STREAM_TIME_BASE) as *const AVRational)
}

/// PURE: read an [`AVDOVIDecoderConfigurationRecord`] out of the raw side-data bytes.
///
/// Split from the pointer walk in [`dovi_conf`] on purpose — this half is the only half worth
/// testing, and it is testable precisely because it never touches FFmpeg. (A test that had to
/// enter the library would take `dlopen`'s `None` branch on Darwin and pass without executing
/// anything, which is the failure shape `docs/agent-reference.md` warns about by name.)
///
/// A SHORT buffer yields `None` rather than a partial record. FFmpeg allocates these with
/// `av_dovi_alloc` and its own demuxers always write all nine bytes, so this cannot happen today
/// — but the record is public ABI whose size "is not a part of the public ABI" by its own header's
/// admission, and reading nine bytes out of a seven-byte allocation is a heap overread that would
/// report a plausible profile number rather than crash. A longer buffer is fine and expected: a
/// future FFmpeg may append fields, and the nine we read keep their meaning.
fn parse_dovi_conf(bytes: &[u8]) -> Option<AVDOVIDecoderConfigurationRecord> {
    if bytes.len() < 9 {
        return None;
    }
    Some(AVDOVIDecoderConfigurationRecord {
        dv_version_major: bytes[0],
        dv_version_minor: bytes[1],
        dv_profile: bytes[2],
        dv_level: bytes[3],
        rpu_present_flag: bytes[4],
        el_present_flag: bytes[5],
        bl_present_flag: bytes[6],
        dv_bl_signal_compatibility_id: bytes[7],
        dv_md_compression: bytes[8],
    })
}

/// The Dolby Vision configuration record attached to this stream, if the demuxer found one.
///
/// `coded_side_data` is the STREAM-level side data (as opposed to `AVPacket::side_data`, which is
/// per-packet), and both bundled demuxers populate it: `mov` from the `dvcC`/`dvvC` sample-entry
/// box, `matroska` from the `DolbyVisionConfiguration` block-additions element. So this answers
/// "what is this file really" for every container the app direct-plays, without decoding a frame.
unsafe fn dovi_conf(cp: *const AVCodecParameters) -> Option<AVDOVIDecoderConfigurationRecord> {
    let (list, n) = ((*cp).coded_side_data, (*cp).nb_coded_side_data);
    if list.is_null() || n <= 0 {
        return None;
    }
    for i in 0..n as usize {
        let sd = &*list.add(i);
        if sd.type_ != AV_PKT_DATA_DOVI_CONF || sd.data.is_null() {
            continue;
        }
        return parse_dovi_conf(std::slice::from_raw_parts(sd.data, sd.size));
    }
    None
}
/// What the container said about a stream's colour: the three H.273 code points from
/// `AVCodecParameters`, plus the mastering display and content light level from the stream's coded
/// side data when the demuxer found them (Matroska's `Colour` element fills them; each only when the
/// file states it). Read for VP9, whose bitstream cannot carry the transfer function.
unsafe fn container_hdr(cp: *const AVCodecParameters) -> crate::player::vp9_hdr::ContainerHdr {
    use crate::player::vp9_hdr::{parse_cll, parse_mastering, ContainerHdr};
    let mut h = ContainerHdr {
        trc: (*cp).color_trc,
        pri: (*cp).color_primaries,
        mc: (*cp).color_space,
        mastering: None,
        cll: None,
    };
    let (list, n) = ((*cp).coded_side_data, (*cp).nb_coded_side_data);
    if list.is_null() || n <= 0 {
        return h;
    }
    for i in 0..n as usize {
        let sd = &*list.add(i);
        if sd.data.is_null() {
            continue;
        }
        let bytes = std::slice::from_raw_parts(sd.data, sd.size);
        match sd.type_ {
            AV_PKT_DATA_MASTERING_DISPLAY_METADATA => h.mastering = parse_mastering(bytes),
            AV_PKT_DATA_CONTENT_LIGHT_LEVEL => h.cll = parse_cll(bytes),
            _ => {}
        }
    }
    h
}
/// The container's own name for this track — `title` (Matroska and most formats), else `name`
/// (MP4's per-track `udta` name box, which is what FFmpeg's mov demuxer exposes it as).
///
/// Deliberately NOT `handler_name`: MP4 sets it to a constant per media type (`"SubtitleHandler"`,
/// `"SoundHandler"`), so it is the same string on every track and reading it would give the picker
/// nine identical sub-lines instead of none — the failure it exists to fix, dressed as a fix.
///
/// The returned `String` is a COPY. The entry points into the stream's dictionary, which
/// `avformat_close_input` frees.
unsafe fn stream_name(s: *mut AVStream) -> String {
    let dict = *((s as *const u8).add(OFF_STREAM_METADATA) as *const *const AVDictionary);
    if dict.is_null() {
        return String::new();
    }
    for key in [c"title", c"name"] {
        let e = av_dict_get(dict, key.as_ptr(), std::ptr::null(), 0);
        if e.is_null() || (*e).value.is_null() {
            continue;
        }
        // `to_string_lossy` rather than a strict decode: these are user-authored tags off a
        // stranger's file, and a mis-encoded byte must cost that character, not the whole name.
        let v = std::ffi::CStr::from_ptr((*e).value)
            .to_string_lossy()
            .trim()
            .to_string();
        if !v.is_empty() {
            return v;
        }
    }
    String::new()
}

/// One tag of the stream's own dictionary (`language`, `title`, ...), trimmed; empty when absent.
unsafe fn stream_tag(s: *mut AVStream, key: &std::ffi::CStr) -> String {
    let dict = *((s as *const u8).add(OFF_STREAM_METADATA) as *const *const AVDictionary);
    if dict.is_null() {
        return String::new();
    }
    let e = av_dict_get(dict, key.as_ptr(), std::ptr::null(), 0);
    if e.is_null() || (*e).value.is_null() {
        return String::new();
    }
    std::ffi::CStr::from_ptr((*e).value).to_string_lossy().trim().to_string()
}

#[inline]
unsafe fn stream_disposition(s: *mut AVStream) -> c_int {
    *((s as *const u8).add(OFF_STREAM_DISPOSITION) as *const c_int)
}

/// Every track's container name, by TYPE, in file order — the two lists
/// `metadata::audio_ordinal` / `metadata::sub_render_ordinal` index into.
///
/// File order is the join, and it is the mapping this file already relies on twice
/// (`nth_audio_stream`, and the subtitle enumeration that feeds `desired_sub_idx`). It is worth
/// stating why that is sound rather than convenient: PMS lists a part's streams in container order
/// too, and both ordinal helpers skip the SIDECAR streams that exist only on the server — which is
/// exactly the set that is not in this file. So position N here and position N there name one
/// track, and a file with no tags simply yields empty strings in the right slots.
unsafe fn track_names(fmt: *mut AVFormatContext) -> (Vec<String>, Vec<String>) {
    let (mut audio, mut subs) = (Vec::new(), Vec::new());
    let streams = (*fmt).streams;
    for i in 0..(*fmt).nb_streams {
        let st = *streams.add(i as usize);
        match (*stream_codecpar(st)).codec_type {
            AVMEDIA_TYPE_AUDIO => audio.push(stream_name(st)),
            AVMEDIA_TYPE_SUBTITLE => subs.push(stream_name(st)),
            _ => {}
        }
    }
    (audio, subs)
}

#[inline]
unsafe fn stream_avg_frame_rate(s: *mut AVStream) -> AVRational {
    *((s as *const u8).add(OFF_STREAM_AVG_FRAME_RATE) as *const AVRational)
}

/// The ffmpeg stream index of the first audio stream whose codec matches `want` (the Load
/// payload's audio codec, e.g. "ac3"), or None. `av_find_best_stream` picks the "highest
/// quality" audio (on an 8-track file it chose DTS over the AC3 default) — but the Load payload
/// carries Media[0].audioCodec, so feeding a different-codec track leaves the audio ES
/// unconfigured and, with audioSync, wedges the video (BufferFull forever). Matching the fed
/// track to the payload codec avoids that.
unsafe fn audio_stream_matching(fmt: *mut AVFormatContext, want: &str) -> Option<c_int> {
    if want.is_empty() {
        return None;
    }
    let streams = (*fmt).streams;
    for i in 0..(*fmt).nb_streams {
        let cp = stream_codecpar(*streams.add(i as usize));
        if (*cp).codec_type == AVMEDIA_TYPE_AUDIO {
            let name = std::ffi::CStr::from_ptr(avcodec_get_name((*cp).codec_id)).to_string_lossy();
            if name.eq_ignore_ascii_case(want) {
                return Some(i as c_int);
            }
        }
    }
    None
}

/// The ffmpeg stream index of the `n`-th audio stream in file order (for native audio-track
/// selection), or None if there are fewer than n+1 audio streams. metadata.audio is filtered
/// in the same file order, so the track menu's 0-based audio index maps 1:1 here.
unsafe fn nth_audio_stream(fmt: *mut AVFormatContext, n: i32) -> Option<c_int> {
    let streams = (*fmt).streams;
    let mut count = 0i32;
    for i in 0..(*fmt).nb_streams {
        let cp = stream_codecpar(*streams.add(i as usize));
        if (*cp).codec_type == AVMEDIA_TYPE_AUDIO {
            if count == n {
                return Some(i as c_int);
            }
            count += 1;
        }
    }
    None
}

// ================================ venc =====================================
// Dev capture stream MPEG1/TS encoder (used by capture.rs when a client's hello
// asks for kind=mpegts): RGBA 960x540 -> sws_scale -> yuv420p AVFrame ->
// mpeg1video (the TV build keeps this encoder) -> mpegts muxer -> custom AVIO
// write callback -> the client socket, raw unframed TS (self-syncing 188B pkts).
//
// ABI strategy (workflow-verified 2026-07-24 against the DEVICE's own libs — the
// AVOption tables inside libavcodec.so.57.89.100 carry the true field offsets):
// every numeric encoder knob goes through av_opt_set ("b"/"g"/"bf"/"maxrate"/
// "bufsize"/"dct"/"time_base"), pix fmts come from av_get_pix_fmt() at runtime
// (AV_PIX_FMT_RGBA is 28 in THIS build — an FF_API_XVMC alias shifts the enum;
// never hardcode it), and only width/height/pix_fmt (AVCodecContext) and
// data/linesize/width/height/format/pts (AVFrame) are raw offset pokes below.
// After avcodec_open2, avcodec_parameters_from_context round-trips the values
// through the already-modeled AVCodecParameters as a runtime ABI self-check —
// a mismatch latches the whole mpeg path off instead of corrupting memory.
// mpeg1video only accepts the fixed MPEG1 frame-rate table: time_base stays
// {1,30} regardless of the actual (jittery, <=30fps) capture cadence; jsmpeg in
// streaming mode ignores timestamps and decodes as data arrives.

// AVCodecContext width/height/pix_fmt USED to be poked here at 124/128/144, under a note claiming
// they had no AVOption. That was wrong — `video_size` (an IMAGE_SIZE option writing width AND
// height) and `pixel_format` are both in libavcodec's own option table — and Venc::open sets them
// by name now, so the constants are gone with the last read of them. Which is the point: all three
// move on webOS 5 (to 92/96/112), and a by-name setter does not care. `ci/ffabi-assert.c` still
// asserts them per major, so if anyone ever needs to poke them again the proven numbers are there.
// AVFrame (avutil 55, 32-bit ARM). pts sits at +104: a 4-byte pad at +100
// 8-aligns the int64 on ARM EABI (the classic AVFrame-on-ARM quirk).
 // u8*[8] — the only one that does not move
#[cfg(target_pointer_width = "32")]
const OFF_FRAME_LINESIZE: usize = 32; // c_int[8]
#[cfg(target_pointer_width = "32")]
const OFF_FRAME_WIDTH: usize = 68;
#[cfg(target_pointer_width = "32")]
const OFF_FRAME_HEIGHT: usize = 72;
#[cfg(target_pointer_width = "32")]
const OFF_FRAME_FORMAT: usize = 80;
#[cfg(target_pointer_width = "32")]
const OFF_FRAME_PTS: usize = 96;
// 64-bit: `data` is eight POINTERS rather than eight 32-bit ones, so `linesize` doubles and
// everything after it follows. `pts` needs no pad here — it is already 8-aligned — which is the
// ARM EABI quirk described above seen from the side where it does not bite.

// All FFmpeg objects are owned and touched by the single capture-encoder thread.

/// Copy the embedded ASS sources before FFmpeg can release them. The codec parameter and
/// dictionary layouts are already compile-asserted against our bundled FFmpeg headers; the
/// attachment media-type constant is asserted beside them. No firmware layout is inferred.
unsafe fn begin_ass_sources(
    media_key: &str,
    fmt: *mut AVFormatContext,
    streams: *mut *mut AVStream,
    subs: &[(c_int, SubKind, *mut AVCodecContext)],
) -> u64 {
    use crate::player::{ass::Font, ass_source};
    let mut headers = Vec::new();
    for (track, (si, kind, _)) in subs.iter().enumerate() {
        if *kind != SubKind::Ass { continue; }
        let cp = stream_codecpar(*streams.add(*si as usize));
        let len = (*cp).extradata_size.max(0) as usize;
        if len > 0 && len <= ass_source::MAX_HEADER_BYTES && !(*cp).extradata.is_null() {
            headers.push((track as i32, std::slice::from_raw_parts((*cp).extradata, len).to_vec()));
        }
    }
    let mut fonts = Vec::new();
    let mut font_budget = crate::player::ass::FontBudget::default();
    let mut font_bytes = 0;
    {
        // External ASS may refer to these attachments even when the container carries no
        // embedded ASS stream. Keep the per-media fonts independently of subtitle selection.
        for i in 0..(*fmt).nb_streams {
            let st = *streams.add(i as usize);
            let cp = stream_codecpar(st);
            let len = (*cp).extradata_size.max(0) as usize;
            if (*cp).codec_type != AVMEDIA_TYPE_ATTACHMENT || len < 4
                || (*cp).extradata.is_null() { continue; }
            let data = std::slice::from_raw_parts((*cp).extradata, len);
            // Accept sfnt/OpenType/TrueType collection signatures, not arbitrary attachments.
            if !matches!(&data[..4], b"\x00\x01\x00\x00" | b"OTTO" | b"ttcf" | b"true") { continue; }
            let dict = *((st as *const u8).add(OFF_STREAM_METADATA) as *const *const AVDictionary);
            let entry = if dict.is_null() { std::ptr::null_mut() }
                else { av_dict_get(dict, c"filename".as_ptr(), std::ptr::null(), 0) };
            let name = if !entry.is_null() && !(*entry).value.is_null() {
                std::ffi::CStr::from_ptr((*entry).value).to_string_lossy().into_owned()
            } else { format!("attachment-{i}.ttf") };
            // Over the budget: skipped before the copy; text naming it falls back to the
            // default face. Charged after the magic check so non-fonts spend nothing.
            if !font_budget.admit(len) { continue; }
            font_bytes += len;
            fonts.push(Font { name, data: data.into() });
        }
        if !headers.is_empty() || !fonts.is_empty() {
            crate::player::log(&format!("ass: embedded tracks={} fonts={} font_bytes={font_bytes}", headers.len(), fonts.len()));
        }
    }
    ass_source::begin(media_key, headers, fonts)
}

static REGISTER: Once = Once::new();
/// One-time `avformat_network_init`. Was also the home of `av_register_all`, which FFmpeg 4.0
/// made a no-op and 5.0 deleted — registration is automatic now, and the bundled build is 9.0.
fn ensure_registered() {
    REGISTER.call_once(|| unsafe {
        avformat_network_init();
    });
}

fn ver(v: c_uint) -> String {
    format!("{}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff)
}

/// Resolve the four FFmpeg libraries by SONAME candidate list. `false` means this device has no
/// FFmpeg we can use, and NOTHING in this module may be called — not even `avformat_version`.
///
/// Kept separate from the version check that follows it because the two failures are different
/// facts about the television and deserve different log lines: "there is no libavformat here at
/// all" versus "there is one and it is the wrong major".
fn load_libraries() -> bool {
    // The bundled FFmpeg lives beside the binary, which is on no library search path — so it is
    // opened by ABSOLUTE PATH. That is not a convenience: webOS 11.2.0 ships FFmpeg 6 itself, and
    // a bare SONAME there could open the television's copy instead of ours.
    let dir = Some(plx_base::paths::app_dir());

    // DEPENDENCY ORDER, and it is load-bearing. libavformat NEEDs libavcodec NEEDs libavutil by
    // SONAME, and these libraries carry no rpath (FFmpeg's configure evals its flags, so
    // -Wl,-rpath,$ORIGIN does not survive — see ci/build-ffmpeg.sh). Loading a dependency FIRST
    // with RTLD_GLOBAL puts it in the global scope under its SONAME, so when the next library
    // names it the loader finds the one we just opened rather than searching the system paths and
    // finding the TV's. Open libavformat first and that guarantee is gone.
    let mut ok = true;
    for (what, verdict) in [
        ("avutil", avutil::load(dir)),
        ("avcodec", avcodec::load(dir)),
        ("avformat", avformat::load(dir)),
    ] {
        match verdict {
            plx_base::dynlib::Loaded::Ok(soname) => {
                plx_base::eventlog::log(&format!("ff: bound {what} -> {soname}"))
            }
            plx_base::dynlib::Loaded::NoLibrary => {
                ok = false;
                plx_base::eventlog::log(&format!(
                    "ff: {what} is MISSING from the app directory — the bundled FFmpeg did not \
                     deploy. Playback will refuse; reinstall the package."
                ));
            }
            plx_base::dynlib::Loaded::Incomplete(soname, n) => {
                ok = false;
                plx_base::eventlog::log(&format!(
                    "ff: {soname} is missing {n} symbol(s) we need — named above"
                ));
            }
        }
    }
    // swscale is NOT required, and treating it as required broke the RELEASE build entirely:
    // `RELEASE=1` drops it from the package (only the dev capture stream's RGBA->YUV conversion
    // uses it), so an all-or-nothing loop over four libraries would have found three, reported
    // failure, and refused to play anything — in the configuration users actually receive, and in
    // no configuration ever tested here.
    match swscale::load(dir) {
        plx_base::dynlib::Loaded::Ok(soname) => {
            SWS_OK.store(true, Ordering::Relaxed);
            plx_base::eventlog::log(&format!("ff: bound swscale -> {soname}"));
        }
        _ => {
            plx_base::eventlog::log("ff: no swscale (expected in a RELEASE build) — dev capture JPEG/MPEG1 off")
        }
    }
    ok
}

/// Did the bundled FFmpeg load and match the ABI table? The diagnostics read-out's first question
/// about the demuxer: false means every playback will refuse before it opens a socket, which from
/// the outside looks identical to a stall.
fn abi_ok() -> bool {
    ABI_OK.load(std::sync::atomic::Ordering::Relaxed)
}

/// Did the CURRENT demux session hand over a single video AU? Distinguishes "the demuxer produced
/// nothing" from "it produced AUs that never reached the decoder" — two stalls that present
/// identically as a spinner. See [`PUSHED_ANY`].
pub fn pushed_any() -> bool {
    PUSHED_ANY.load(std::sync::atomic::Ordering::Relaxed)
}

/// The three FFmpeg majors actually bound, for the read-out's build line. Zeroes before [`boot`]
/// has run or when the libraries did not load.
pub fn majors() -> (u32, u32, u32) {
    if !abi_ok() {
        return (0, 0, 0);
    }
    unsafe {
        (
            avformat_version() >> 16,
            avcodec_version() >> 16,
            avutil_version() >> 16,
        )
    }
}

/// Boot smoke test + optional ABI probe. Called once at startup.
pub fn boot() {
    if !load_libraries() {
        ABI_OK.store(false, std::sync::atomic::Ordering::Relaxed);
        plx_base::eventlog::log("ff: FFmpeg unavailable — the app runs, playback will refuse");
        return;
    }
    unsafe {
        let (fmt, cod, utl) = (avformat_version(), avcodec_version(), avutil_version());
        crate::player::log(&format!(
            "ff: avformat={} avcodec={} avutil={}",
            ver(fmt),
            ver(cod),
            ver(utl)
        ));
        // THE MAJORS ARE THE ABI. Everything below reads FFmpeg structs at offsets that are fixed
        // per major: FFmpeg guarantees layout only within one, and majors reorder.
        //
        // One version, checked against the one we built for. This used to SELECT between two
        // offset tables because the app read the television's FFmpeg and every webOS release
        // shipped a different one; bundling made that a single equality. The refusal still
        // matters: a wrong offset does not fail, it succeeds with garbage. Reading `codecpar` at
        // the n3.3 offset on a modern AVStream lands hundreds of bytes past the struct and
        // dereferences whatever it finds, on a device with no debugger.
        // The version we BUILT against, not a version we hope to find. These libraries ship
        // inside the package under a -plx SONAME, so a mismatch means the deployed payload is
        // stale or someone put a different file there — not a firmware difference.
        let bad = (fmt >> 16, cod >> 16, utl >> 16) != (63, 63, 61);
        ABI_OK.store(!bad, Ordering::Relaxed);
        if bad {
            plx_base::eventlog::log(
                "ff: BUNDLED FFmpeg is not the one this build expects (want avformat 63 / \
                 avcodec 63 / avutil 61) — the app directory holds a stale or foreign \
                 libav*-plx; refusing to demux",
            );
        }
    }
}

/// One audio or subtitle track of a local file, as its container describes it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LocalTrack {
    /// FFmpeg's codec name (`eac3`, `subrip`, `hdmv_pgs_subtitle`, ...).
    pub codec: String,
    /// The `language` tag — an ISO 639-2 code (`eng`) in Matroska and MP4; empty when untagged.
    pub lang: String,
    /// The track's own name ([`stream_name`]).
    pub title: String,
    pub channels: i32,
    pub default: bool,
    pub forced: bool,
    pub sdh: bool,
}

/// What [`probe_local`] learned about a file on this machine's disk: everything the Load
/// declaration needs, which for a Plex item comes from the server's decision instead.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalProbe {
    /// FFmpeg's codec name of the video stream the demuxer will pick (`h264`, `hevc`, ...).
    pub vcodec: String,
    /// FFmpeg's codec name of the first audio stream, in file order, that the pipeline can carry
    /// (`engine::audio_payload_codec` names it); empty when the file has none.
    pub acodec: String,
    /// Every audio stream's codec name, in file order — what the refusal names when none fits.
    pub audio_codecs: Vec<String>,
    /// Every audio and subtitle track, in file order: what the player's track menu lists.
    pub audio: Vec<LocalTrack>,
    pub subs: Vec<LocalTrack>,
    pub width: u16,
    pub height: u16,
    /// Frames per second, 0 when the container does not say.
    pub fps: f64,
    pub duration_ns: i64,
}

/// Open a local file with the bundled libavformat (its one protocol is `file`) and read what a
/// Load declaration needs. Blocking — call it off the frame thread.
pub fn probe_local(path: &std::path::Path) -> Result<LocalProbe, String> {
    if !ABI_OK.load(Ordering::Relaxed) {
        return Err("FFmpeg is unavailable".into());
    }
    ensure_registered();
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| "path contains a NUL byte".to_string())?;
    unsafe {
        let mut fmt: *mut AVFormatContext = std::ptr::null_mut();
        let r = avformat_open_input(&mut fmt, c.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut());
        if r < 0 || fmt.is_null() {
            return Err(format!("not a readable media file (avformat_open_input {r})"));
        }
        let result = (|| {
            if avformat_find_stream_info(fmt, std::ptr::null_mut()) < 0 {
                return Err("could not read the streams".to_string());
            }
            let vi = av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, -1, -1, std::ptr::null_mut(), 0);
            if vi < 0 {
                return Err("no video stream".to_string());
            }
            let vcp = stream_codecpar(*(*fmt).streams.add(vi as usize));
            let name = |id: c_int| std::ffi::CStr::from_ptr(avcodec_get_name(id)).to_string_lossy().into_owned();
            let (mut audio, mut subs) = (Vec::new(), Vec::new());
            for i in 0..(*fmt).nb_streams {
                let st = *(*fmt).streams.add(i as usize);
                let cp = stream_codecpar(st);
                let disposition = stream_disposition(st);
                let track = || LocalTrack {
                    codec: name((*cp).codec_id),
                    lang: stream_tag(st, c"language"),
                    title: stream_name(st),
                    channels: (*cp).ch_layout.nb_channels,
                    default: disposition & AV_DISPOSITION_DEFAULT != 0,
                    forced: disposition & AV_DISPOSITION_FORCED != 0,
                    sdh: disposition & AV_DISPOSITION_HEARING_IMPAIRED != 0,
                };
                match (*cp).codec_type {
                    AVMEDIA_TYPE_AUDIO => audio.push(track()),
                    AVMEDIA_TYPE_SUBTITLE => subs.push(track()),
                    _ => {}
                }
            }
            let audio_codecs: Vec<String> = audio.iter().map(|t: &LocalTrack| t.codec.clone()).collect();
            // FILE order, not codec preference: the first track is the one the file's author
            // put first (usually the main mix), and a commentary must never win on its codec.
            let acodec = audio_codecs
                .iter()
                .find(|a| crate::player::engine::audio_payload_codec(a).is_some())
                .cloned()
                .unwrap_or_default();
            let as_fps = |r: AVRational| (r.num > 0 && r.den > 0).then(|| r.num as f64 / r.den as f64);
            let fps = as_fps((*vcp).framerate)
                .or_else(|| as_fps(stream_avg_frame_rate(*(*fmt).streams.add(vi as usize))))
                .unwrap_or(0.0);
            let clamp = |v: c_int| u16::try_from(v.max(0)).unwrap_or(u16::MAX);
            Ok(LocalProbe {
                vcodec: name((*vcp).codec_id),
                acodec,
                audio_codecs,
                audio,
                subs,
                width: clamp((*vcp).width),
                height: clamp((*vcp).height),
                fps,
                duration_ns: fmt_duration(fmt).max(0).saturating_mul(1_000),
            })
        })();
        avformat_close_input(&mut fmt);
        result
    }
}

// ==== the demuxer (Phases B–D) ===================================================

// whence values for the AVIO seek callback
const SEEK_SET: c_int = 0;
const SEEK_CUR: c_int = 1;
const SEEK_END: c_int = 2;
const AVSEEK_SIZE: c_int = 0x10000;

#[path = "ff_subs.rs"]
mod subs;
use subs::{SubKind, SubTracks};
#[cfg(test)]
use subs::{decode_bitmap_cue, open_sub_decoder, sub_kind};

/// AVIO backing state: the file libavformat reads through, and the byte it reads next. Boxed so
/// its address is stable for the C callbacks.
struct AvioState {
    /// A file on this machine's disk ([`crate::localfile`]): positional reads, so a seek moves
    /// `off` and reopens nothing, and nothing is ever in flight between two reads.
    file: Box<crate::localfile::LocalFile>,
    aq: *mut AuQueue,
    off: i64,
    size: i64,
    /// A read failed during the current libavformat operation. FFmpeg may heal it through
    /// `seek_cb`; only an operation that still returns failure publishes it to the player.
    io_failed: bool,
}

/// AVIOContext leading fields, so we can free avio->buffer manually (FFmpeg 3.3 has no
/// avio_context_free). `buffer` sits at +4, right after `av_class`.
#[repr(C)]
struct AVIOCtxHead {
    av_class: *const c_void,
    buffer: *mut u8,
}

extern "C" fn read_cb(op: *mut c_void, dst: *mut u8, n: c_int) -> c_int {
    unsafe {
        let s = &mut *(op as *mut AvioState);
        // Teardown aborts the lanes before it joins this thread: answer it as the end.
        if crate::aq::aq_is_aborted(s.aq) {
            return AVERROR_EOF;
        }
        if dst.is_null() || n <= 0 {
            return 0;
        }
        let r = s.file.read_at(std::slice::from_raw_parts_mut(dst, n as usize), s.off);
        if r < 0 {
            crate::player::log(&format!("ff: local read FAILED at {} (errno {})", s.off, -r));
            s.io_failed = true;
            return AVERROR_IO;
        }
        if r == 0 {
            return AVERROR_EOF;
        }
        s.off += r as i64;
        r
    }
}

extern "C" fn seek_cb(op: *mut c_void, offset: i64, whence: c_int) -> i64 {
    unsafe {
        let s = &mut *(op as *mut AvioState);
        if whence == AVSEEK_SIZE {
            return s.size;
        }
        if crate::aq::aq_is_aborted(s.aq) {
            return -1;
        }
        let target = match whence {
            SEEK_SET => offset,
            SEEK_CUR => s.off + offset,
            SEEK_END => s.size + offset,
            _ => return -1,
        };
        if target < 0 {
            return -1;
        }
        // Positional reads: the next one simply starts at `target`. This may be libavformat
        // healing a failed read; a later failure arms the bit again.
        s.off = target;
        s.io_failed = false;
        target
    }
}

/// Settle one `av_read_frame` result against errors observed by its AVIO callbacks.
fn frame_read_failed(state: &mut AvioState, result: c_int) -> bool {
    if result >= 0 {
        return false;
    }
    if state.io_failed {
        crate::player::log(&format!("ff: the file could not be read during av_read_frame r={result}"));
        SHARED.demux_io_failed.store(true, Ordering::Release);
    }
    true
}

#[inline]
unsafe fn pts_ns(pkt: *const AVPacket, st: *mut AVStream) -> i64 {
    let t = if (*pkt).pts != AV_NOPTS_VALUE {
        (*pkt).pts
    } else {
        (*pkt).dts
    };
    if t == AV_NOPTS_VALUE {
        return 0;
    }
    av_rescale_q(t, stream_time_base(st), NS_TB)
}

unsafe fn free_ptr(p: *mut c_void) {
    let mut q = p;
    av_freep(&mut q as *mut *mut c_void as *mut c_void);
}

/// Free a custom AVIOContext (buffer + context): FFmpeg 3.3 has no avio_context_free and
/// avformat_close_input does not free a caller-set pb.
unsafe fn free_avio(avio: *mut AVIOContext) {
    if avio.is_null() {
        return;
    }
    let head = avio as *mut AVIOCtxHead;
    av_freep(std::ptr::addr_of_mut!((*head).buffer) as *mut c_void); // frees + NULLs avio->buffer
    let mut p = avio;
    av_freep(&mut p as *mut *mut AVIOContext as *mut c_void); // frees + NULLs the context
}

// -- AV1 framing ------------------------------------------------------------------------
//
// The TV pipeline is handed no codec_data for AV1 (libpf's AV1 caps carry none, and the pipeline
// has no av1parse element), so the configuration has to travel in-band. These helpers turn the
// demuxed packet into a Section-5 temporal unit: a temporal delimiter first, the av1C config OBUs
// in front of each keyframe, and every OBU carrying a size field. Everything here is pure, so
// the framing is tested on the host (ff_packet_framing_tests.rs).

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_TEMPORAL_DELIMITER: u8 = 2;
const OBU_METADATA: u8 = 5;
/// A temporal delimiter OBU with an empty payload: header 0x12 (type 2, size field), size 0.
const AV1_TD: [u8; 2] = [0x12, 0x00];

/// Read one leb128 value from the front of `b` (AV1 section 4.10.5), returning the value and the
/// number of bytes it took. A run with no terminator inside eight bytes, a truncated run, and a
/// value wider than 32 bits are all rejected: the spec bounds every leb128 field to 32 bits.
fn read_leb128(b: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, &byte) in b.iter().take(8).enumerate() {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return (value <= u64::from(u32::MAX)).then_some((value, i + 1));
        }
    }
    None
}

/// Append `v` as a minimal leb128 encoding.
fn write_leb128(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// One OBU inside a byte run: where its header starts, how long the header is (one byte, plus
/// one when the extension flag is set), where its payload starts, and where it ends (exclusive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ObuSpan {
    start: usize,
    header_len: usize,
    payload_start: usize,
    end: usize,
    obu_type: u8,
    has_size: bool,
}

/// Split an OBU run into its OBUs. An OBU without a size field runs to the end of the run and
/// is therefore the last one. Returns None for a set forbidden bit, a truncated header or size,
/// or a payload that overruns the run.
fn walk_obus(b: &[u8]) -> Option<Vec<ObuSpan>> {
    let mut spans = Vec::new();
    let mut p = 0usize;
    while p < b.len() {
        let h = b[p];
        if h & 0x80 != 0 {
            return None;
        }
        let obu_type = (h >> 3) & 0x0f;
        let header_len = 1 + usize::from((h >> 2) & 1);
        let has_size = (h >> 1) & 1 == 1;
        if p + header_len > b.len() {
            return None;
        }
        let body = p + header_len;
        let (payload_start, end) = if has_size {
            let (size, n) = read_leb128(&b[body..])?;
            let payload_start = body + n;
            // u64 arithmetic: on the 32-bit TV `payload_start + size` could wrap a usize.
            if payload_start as u64 + size > b.len() as u64 {
                return None;
            }
            (payload_start, payload_start + size as usize)
        } else {
            (body, b.len())
        };
        spans.push(ObuSpan {
            start: p,
            header_len,
            payload_start,
            end,
            obu_type,
            has_size,
        });
        p = end;
    }
    Some(spans)
}

/// Append one OBU with a size field. An OBU that already has one is copied verbatim; a sizeless
/// one (only ever the last in its run) gets the minimal leb128 of its payload length.
fn push_obu_sized(b: &[u8], s: &ObuSpan, out: &mut Vec<u8>) {
    if s.has_size {
        out.extend_from_slice(&b[s.start..s.end]);
        return;
    }
    out.push(b[s.start] | 0x02);
    if s.header_len == 2 {
        out.push(b[s.start + 1]);
    }
    write_leb128((s.end - s.payload_start) as u64, out);
    out.extend_from_slice(&b[s.payload_start..s.end]);
}

/// The av1C configOBUs to insert in front of a keyframe: every sequence header, then every
/// metadata OBU (HDR10 MDCV/CLL may live only in the record). Empty when the record carries no
/// sequence header, because metadata alone is never inserted. Accepts both the av1C record
/// (marker and version byte 0x81, three bytes of fixed fields, then the OBUs) and a bare OBU run.
fn parse_av1c(ed: &[u8]) -> Vec<u8> {
    let obus = if ed.len() >= 4 && ed[0] == 0x81 {
        &ed[4..]
    } else if !ed.is_empty() && ed[0] & 0x80 == 0 {
        ed
    } else {
        return Vec::new();
    };
    let Some(spans) = walk_obus(obus) else {
        return Vec::new();
    };
    if !spans.iter().any(|s| s.obu_type == OBU_SEQUENCE_HEADER) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for s in spans.iter().filter(|s| s.obu_type == OBU_SEQUENCE_HEADER) {
        push_obu_sized(obus, s, &mut out);
    }
    for s in spans.iter().filter(|s| s.obu_type == OBU_METADATA) {
        push_obu_sized(obus, s, &mut out);
    }
    out
}

/// Which pieces of the AV1 temporal-unit framing are emitted. DEFAULT is the conformant form.
/// The other settings exist so the reporter can A/B the Realtek decoder's expectations on a
/// device; they come from the `av1obu` dev trigger and are never set in a release build.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Av1Framing {
    /// Prepend a temporal delimiter when the packet has none.
    td: bool,
    /// Prepend the av1C config block on keyframes that carry no sequence header.
    seq: bool,
    /// Pass the packet bytes through exactly as demuxed.
    raw: bool,
}

impl Av1Framing {
    const DEFAULT: Av1Framing = Av1Framing {
        td: true,
        seq: true,
        raw: false,
    };

    /// Parse the `av1obu` trigger. Tokens are separated by commas or whitespace; each one sets
    /// one flag independently, and unknown tokens are ignored.
    fn from_trigger(s: Option<&str>) -> Av1Framing {
        let mut f = Self::DEFAULT;
        for tok in s
            .unwrap_or("")
            .split(|c: char| c == ',' || c.is_ascii_whitespace())
            .filter(|t| !t.is_empty())
        {
            match tok {
                "notd" => f.td = false,
                "noseq" => f.seq = false,
                "raw" => f.raw = true,
                _ => {}
            }
        }
        f
    }
}

/// Frame one demuxed AV1 packet as a temporal unit in `out`, returning `key` unchanged.
/// A packet that does not parse as OBUs is copied through untouched: it is never dropped.
fn av1_packet_to_tu(pkt: &[u8], key: bool, cfg: &[u8], f: Av1Framing, out: &mut Vec<u8>) -> bool {
    out.clear();
    let spans = match walk_obus(pkt) {
        Some(spans) if !f.raw => spans,
        _ => {
            out.extend_from_slice(pkt);
            return key;
        }
    };
    let has_td = spans
        .first()
        .is_some_and(|s| s.obu_type == OBU_TEMPORAL_DELIMITER);
    let has_seq = spans.iter().any(|s| s.obu_type == OBU_SEQUENCE_HEADER);
    let mut rest = &spans[..];
    if has_td {
        push_obu_sized(pkt, &spans[0], out);
        rest = &spans[1..];
    } else if f.td {
        out.extend_from_slice(&AV1_TD);
    }
    if key && f.seq && !has_seq && !cfg.is_empty() {
        out.extend_from_slice(cfg);
    }
    if let Some((last, body)) = rest.split_last() {
        for s in body {
            out.extend_from_slice(&pkt[s.start..s.end]);
        }
        push_obu_sized(pkt, last, out);
    }
    key
}

/// How a demuxed video packet is turned into the access unit the television's pipeline is fed:
/// the one place a codec id is mapped to a framing, so a codec with no arm is REFUSED instead of
/// falling into the Annex-B path (which on a bitstream that is not H.264/HEVC produces garbage the
/// decoder rejects with nothing in the log naming the cause).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VideoFraming {
    /// Length-prefixed NAL units rewritten to Annex-B with the parameter sets prepended at each
    /// keyframe (`packet_to_annexb`).
    AnnexB { hevc: bool },
    /// Section-5 temporal units with the av1C configuration in-band (`av1_packet_to_tu`).
    Av1,
    /// The packet unchanged (`vp9_packet_to_frame`).
    Vp9,
}

impl VideoFraming {
    pub(crate) fn of_codec_id(id: c_int) -> Option<Self> {
        match id {
            AV_CODEC_ID_H264 => Some(Self::AnnexB { hevc: false }),
            AV_CODEC_ID_HEVC => Some(Self::AnnexB { hevc: true }),
            AV_CODEC_ID_AV1 => Some(Self::Av1),
            AV_CODEC_ID_VP9 => Some(Self::Vp9),
            _ => None,
        }
    }

    /// The Load declaration this framing belongs with. The Load is sent before the demuxer opens the
    /// file, so a disagreement can only abort and say so afterwards; that still beats the black
    /// screen it would otherwise be.
    pub(crate) fn load_video(self) -> crate::player::engine::LoadVideo {
        use crate::player::engine::LoadVideo;
        match self {
            Self::AnnexB { hevc: false } => LoadVideo::H264,
            Self::AnnexB { hevc: true } => LoadVideo::H265,
            Self::Av1 => LoadVideo::Av1,
            Self::Vp9 => LoadVideo::Vp9,
        }
    }
}

/// Hand one demuxed VP9 packet to the decoder as it came, returning `key` unchanged.
///
/// A VP9 frame is self-describing: every keyframe carries its profile, size and colour config in
/// its own uncompressed header, and libpf's `video/x-vp9` appsrc caps have no `codec_data` to
/// fill, so there is nothing to prepend (compare `av1_packet_to_tu`, where the sequence header
/// has to be put back in-band). A Matroska/MP4 packet may be a SUPERFRAME, a shown frame and its
/// hidden reference frames under one trailing index; it is passed whole, the way a WebM-aware
/// hardware decoder is fed.
fn vp9_packet_to_frame(pkt: &[u8], key: bool, out: &mut Vec<u8>) -> bool {
    out.clear();
    out.extend_from_slice(pkt);
    key
}

/// Parse an avcC (H264) / hvcC (HEVC) extradata record into a ready-to-prepend Annex-B
/// parameter-set blob (VPS/SPS/PPS with 4-byte start codes) + the NAL length-prefix size.
/// (Ported from the retired mkv.rs demuxer) — the format the Starfish decoder wants.
unsafe fn parse_extradata(ed: *const u8, len: usize, is_hevc: bool) -> (Vec<u8>, usize) {
    let mut blob = Vec::new();
    if ed.is_null() || len < 7 {
        return (blob, 4);
    }
    let e = std::slice::from_raw_parts(ed, len);
    let sc = [0u8, 0, 0, 1];
    if is_hevc {
        // HEVCDecoderConfigurationRecord: ver@0==1, nal_len@21, numArrays@22, arrays@23.
        if len < 23 || e[0] != 1 {
            return (blob, 4);
        }
        let nls = (e[21] & 3) as usize + 1;
        let narr = e[22] as usize;
        let mut p = 23usize;
        for _ in 0..narr {
            if p + 3 > len {
                break;
            }
            // array header: [completeness|reserved|NAL_type(&0x3f)] u16 count.
            // ONLY VPS(32)/SPS(33)/PPS(34) belong in the prepend blob; skip SEI(39/40) and
            // anything else — a stray SEI here corrupts the parameter-set sequence.
            let keep = matches!(e[p] & 0x3f, 32 | 33 | 34);
            let cnt = ((e[p + 1] as usize) << 8) | e[p + 2] as usize;
            p += 3;
            for _ in 0..cnt {
                if p + 2 > len {
                    break;
                }
                let nl = ((e[p] as usize) << 8) | e[p + 1] as usize;
                p += 2;
                if nl == 0 || p + nl > len {
                    break;
                }
                if keep {
                    blob.extend_from_slice(&sc);
                    blob.extend_from_slice(&e[p..p + nl]);
                }
                p += nl;
            }
        }
        (blob, nls)
    } else {
        // AVCDecoderConfigurationRecord: ver@0==1, nal_len@4, SPS list @5, then PPS list.
        if e[0] != 1 {
            return (blob, 4);
        }
        let nls = (e[4] & 3) as usize + 1;
        let nsps = (e[5] & 0x1f) as usize;
        let mut p = 6usize;
        for _ in 0..nsps {
            if p + 2 > len {
                break;
            }
            let nl = ((e[p] as usize) << 8) | e[p + 1] as usize;
            p += 2;
            if nl == 0 || p + nl > len {
                break;
            }
            blob.extend_from_slice(&sc);
            blob.extend_from_slice(&e[p..p + nl]);
            p += nl;
        }
        if p < len {
            let npps = e[p] as usize;
            p += 1;
            for _ in 0..npps {
                if p + 2 > len {
                    break;
                }
                let nl = ((e[p] as usize) << 8) | e[p + 1] as usize;
                p += 2;
                if nl == 0 || p + nl > len {
                    break;
                }
                blob.extend_from_slice(&sc);
                blob.extend_from_slice(&e[p..p + nl]);
                p += nl;
            }
        }
        (blob, nls)
    }
}

static SEI_STRIPPED: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// True if this HEVC NAL is an SEI (prefix=39 / suffix=40) carrying a user-data-registered
/// ITU-T T.35 HDR10+ dynamic-metadata message (payloadType 4, country_code 0xB5, terminal
/// provider 0x003C). webOS's HEVC decoder does NOT support per-frame HDR10+ metadata and
/// stalls on it — Kodi strips it unconditionally ("webOS doesn't support HDR10+ and it can
/// cause issues"). `nal` is the NAL payload WITHOUT the start code (starts at the 2-byte
/// NAL header). Static HDR10 mastering-display / CLL SEI (payloadType 137/144) is kept.
fn is_hdr10plus_sei(nal: &[u8]) -> bool {
    if nal.len() < 2 {
        return false;
    }
    let nal_type = (nal[0] >> 1) & 0x3f;
    if nal_type != 39 && nal_type != 40 {
        return false;
    }
    let mut p = 2usize; // SEI RBSP starts after the 2-byte NAL header
    let mut ptype = 0usize; // payloadType (0xFF-run + terminator)
    while p < nal.len() && nal[p] == 0xff {
        ptype += 255;
        p += 1;
    }
    if p >= nal.len() {
        return false;
    }
    ptype += nal[p] as usize;
    p += 1;
    while p < nal.len() && nal[p] == 0xff {
        p += 1; // skip payloadSize (0xFF-run + terminator); value unused
    }
    p += 1;
    // user_data_registered_itu_t_t35 == 4; T.35 header: country 0xB5, provider 0x003C
    ptype == 4 && p + 3 <= nal.len() && nal[p] == 0xb5 && nal[p + 1] == 0x00 && nal[p + 2] == 0x3c
}

/// End offset of a NAL of length `nl` starting at `i`, or None if it is empty or would run
/// past `size`. **The width is load-bearing**: `usize` is 32 bits on the TV, `nls` is up to 4,
/// so a hostile/corrupt length up to `0xFFFF_FFFF` makes the natural `i + nl > size` guard WRAP
/// to a small number, pass its own bounds check, and panic the demux thread inside the slice —
/// killing the producer before it can EOF the queues, which hangs playback with no error path.
/// Computing in `u64` makes the bound hold on every target. Never inline this back to `i + nl`.
fn nal_end(i: usize, nl: usize, size: usize) -> Option<usize> {
    if nl == 0 {
        return None;
    }
    let end = i as u64 + nl as u64;
    if end > size as u64 {
        return None;
    }
    Some(end as usize)
}

/// Convert one length-prefixed video packet to Annex-B (4-byte start codes) into `out`,
/// prepending `param` (VPS/SPS/PPS) when the AU is a keyframe (H264 IDR type 5 / HEVC IRAP
/// types 16-23). Returns true if it is a keyframe. Mirrors mkv_handle_block.
unsafe fn packet_to_annexb(
    data: *const u8,
    size: usize,
    nls: usize,
    is_hevc: bool,
    param: &[u8],
    out: &mut Vec<u8>,
) -> bool {
    out.clear();
    if data.is_null() || size < nls + 1 {
        return false;
    }
    let d = std::slice::from_raw_parts(data, size);
    // pass 1: is this AU a keyframe?
    let mut is_key = false;
    let mut i = 0usize;
    while i + nls <= size {
        let mut nl = 0usize;
        for k in 0..nls {
            nl = (nl << 8) | d[i + k] as usize;
        }
        i += nls;
        if nal_end(i, nl, size).is_none() {
            break;
        }
        let b0 = d[i];
        let key = if is_hevc {
            (16..=23).contains(&((b0 >> 1) & 0x3f))
        } else {
            (b0 & 0x1f) == 5
        };
        if key {
            is_key = true;
            break;
        }
        i += nl;
    }
    if is_key {
        out.extend_from_slice(param);
    }
    // pass 2: emit each NAL as 00 00 00 01 + bytes, dropping per-frame HDR10+ SEI (HEVC)
    let sc = [0u8, 0, 0, 1];
    let mut i = 0usize;
    while i + nls <= size {
        let mut nl = 0usize;
        for k in 0..nls {
            nl = (nl << 8) | d[i + k] as usize;
        }
        i += nls;
        let Some(end) = nal_end(i, nl, size) else {
            break;
        };
        let nal = &d[i..end];
        if is_hevc && is_hdr10plus_sei(nal) {
            let n = SEI_STRIPPED.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 3 || n % 500 == 0 {
                crate::player::log(&format!("ff: stripped HDR10+ SEI #{n} ({nl} bytes)"));
            }
            i += nl;
            continue;
        }
        out.extend_from_slice(&sc);
        out.extend_from_slice(nal);
        i += nl;
    }
    is_key
}

static DIAG_FIRST: AtomicBool = AtomicBool::new(true);

/// A DTS stream's profile (`AV_PROFILE_DTS_*` in `libavcodec/defs.h`), named for the log.
fn dts_profile_name(profile: c_int) -> &'static str {
    match profile {
        30 => "DTS-ES",
        40 => "DTS 96/24",
        50 => "DTS-HD HRA",
        60 => "DTS-HD MA",
        61 => "DTS-HD MA + DTS:X",
        62 => "DTS-HD MA + DTS:X IMAX",
        70 => "DTS Express",
        _ => "DTS",
    }
}

/// Extract one DTS core access unit, dropping the DTS-HD extension substream. Matroska and
/// MOV demux return the complete packet; our bundled FFmpeg 9.0 has neither the dca parser nor
/// dca_core BSF enabled. The core size is the 14-bit FSIZE + 1 from its normalized header
/// (ETSI TS 102 114; FFmpeg 9.0 libavcodec/bsf/dca_core.c). No new FFmpeg ABI is needed.
/// Normalize the four core sync formats to 16-bit big endian before measuring. Extension-only,
/// truncated and malformed packets are rejected instead of sending HD data to the core decoder.
fn dts_core_packet(packet: &[u8]) -> Option<std::borrow::Cow<'_, [u8]>> {
    use std::borrow::Cow;
    let sync: [u8; 4] = packet.get(..4)?.try_into().ok()?;
    let normalized = match sync {
        [0x7f, 0xfe, 0x80, 0x01] => Cow::Borrowed(packet),
        [0xfe, 0x7f, 0x01, 0x80] => {
            let mut bytes = packet.to_vec();
            for word in bytes.chunks_exact_mut(2) { word.swap(0, 1); }
            Cow::Owned(bytes)
        }
        [0x1f, 0xff, 0xe8, 0x00] | [0xff, 0x1f, 0x00, 0xe8] => {
            let little = sync[0] == 0xff;
            let mut bytes = Vec::with_capacity(packet.len() * 7 / 8);
            let (mut bits, mut count) = (0u32, 0u32);
            for word in packet.chunks_exact(2) {
                let word = if little { u16::from_le_bytes([word[0], word[1]]) }
                    else { u16::from_be_bytes([word[0], word[1]]) };
                bits = (bits << 14) | u32::from(word & 0x3fff);
                count += 14;
                while count >= 8 {
                    count -= 8;
                    bytes.push((bits >> count) as u8);
                }
            }
            Cow::Owned(bytes)
        }
        _ => return None,
    };
    let header = normalized.get(..10)?;
    let size = (((usize::from(header[5]) & 3) << 12)
        | (usize::from(header[6]) << 4) | (usize::from(header[7]) >> 4)) + 1;
    // The standard's minimum core frame length is 96 bytes. Never accept a size that points
    // back into the header or beyond the packet, including a truncated 14-bit word.
    if size < 96 || size > normalized.len() { return None; }
    Some(match normalized {
        Cow::Borrowed(bytes) => Cow::Borrowed(&bytes[..size]),
        Cow::Owned(mut bytes) => { bytes.truncate(size); Cow::Owned(bytes) }
    })
}

#[cfg(test)]
mod dts_core_tests {
    use super::dts_core_packet;

    fn core() -> Vec<u8> {
        let mut frame = vec![0x55; 128];
        frame[..10].copy_from_slice(&[0x7f, 0xfe, 0x80, 0x01, 0xfc, 0x00, 0x07, 0xf0, 0, 0]);
        frame
    }

    #[test]
    fn the_log_names_the_dts_profile_by_ffmpegs_numbers() {
        assert_eq!(super::dts_profile_name(60), "DTS-HD MA");
        assert_eq!(super::dts_profile_name(50), "DTS-HD HRA");
        assert_eq!(super::dts_profile_name(20), "DTS");
        assert_eq!(super::dts_profile_name(-99), "DTS", "FF_PROFILE_UNKNOWN");
    }

    #[test]
    fn preserves_the_generated_dts_5_1_access_unit() {
        let frame = include_bytes!("../../../tests/fixtures/dts-core-frame.dts");
        assert_eq!(dts_core_packet(frame).unwrap().as_ref(), frame);
        let mut with_extension = frame.to_vec();
        with_extension.extend_from_slice(&[0x64, 0x58, 0x20, 0x25, 0, 0, 0, 0]);
        assert_eq!(dts_core_packet(&with_extension).unwrap().as_ref(), frame);
    }

    #[test]
    fn preserves_core_and_strips_hd_extensions() {
        let core = core();
        assert_eq!(dts_core_packet(&core).unwrap().as_ref(), core);
        let mut hd = core.clone();
        hd.extend_from_slice(&[0x64, 0x58, 0x20, 0x25, 0x01, 0x02, 0x03, 0x04]);
        assert_eq!(dts_core_packet(&hd).unwrap().as_ref(), core);
    }

    #[test]
    fn refuses_extension_only_truncated_and_invalid_sizes() {
        assert!(dts_core_packet(&[0x64, 0x58, 0x20, 0x25]).is_none());
        let core = core();
        for n in 0..core.len() { assert!(dts_core_packet(&core[..n]).is_none()); }
        let mut invalid = core;
        invalid[6] = 0;
        invalid[7] = 0;
        assert!(dts_core_packet(&invalid).is_none());
    }

    #[test]
    fn normalizes_word_swapped_and_packed_core_formats() {
        let core = core();
        let mut swapped = core.clone();
        for word in swapped.chunks_exact_mut(2) { word.swap(0, 1); }
        assert_eq!(dts_core_packet(&swapped).unwrap().as_ref(), core);
        let mut packed = Vec::new();
        let (mut bits, mut count) = (0u32, 0u32);
        for &byte in core.iter().chain([0u8; 2].iter()) {
            bits = (bits << 8) | u32::from(byte);
            count += 8;
            if count >= 14 {
                count -= 14;
                let mut word = ((bits >> count) & 0x3fff) as u16;
                if packed.len() == 2 { word |= 0xc000; }
                packed.extend_from_slice(&word.to_be_bytes());
            }
        }
        assert_eq!(dts_core_packet(&packed).unwrap().as_ref(), core);
        for word in packed.chunks_exact_mut(2) { word.swap(0, 1); }
        assert_eq!(dts_core_packet(&packed).unwrap().as_ref(), core);
    }
}

/// ADTS sampling_frequency_index (4 bits) for a sample rate; None if not a standard AAC rate.
fn adts_freq_index(rate: c_int) -> Option<u8> {
    Some(match rate {
        96000 => 0,
        88200 => 1,
        64000 => 2,
        48000 => 3,
        44100 => 4,
        32000 => 5,
        24000 => 6,
        22050 => 7,
        16000 => 8,
        12000 => 9,
        11025 => 10,
        8000 => 11,
        7350 => 12,
        _ => return None,
    })
}

/// (freq_idx, chan_cfg) for the ADTS header, read from the stream's AudioSpecificConfig when
/// present. HE-AAC is why this parses the ASC instead of trusting codecpar: the ASC's FIRST
/// samplingFrequencyIndex is the AAC **core** rate (e.g. 24 kHz) while `codecpar.sample_rate`
/// reports the SBR **output** rate (48 kHz). ADTS must carry the core rate (+ LC profile, SBR
/// stays implicit) so the decoder up-samples through the SBR extension — a 48 kHz header makes
/// it decode each frame as 1024 plain-LC samples into a 2048-sample slot, an audible gap/crackle
/// every frame (Maxton Hall, HE-AAC 5.1). Falls back to codecpar for ASC-less streams.
unsafe fn adts_params(acp: *const AVCodecParameters) -> Option<(u8, u8)> {
    let chan_fallback = {
        let ch = (*acp).ch_layout.nb_channels;
        if (1..=7).contains(&ch) { ch as u8 } else { 2 }
    };
    // AudioSpecificConfig: aot(5) freqIdx(4) [+24-bit rate if idx==15] chanCfg(4) …
    let (ed, n) = ((*acp).extradata, (*acp).extradata_size);
    if !ed.is_null() && n >= 2 {
        let (b0, b1) = (*ed as u32, *ed.add(1) as u32);
        let aot = (b0 >> 3) & 0x1F;
        let freq_idx = (((b0 & 0x07) << 1) | (b1 >> 7)) as u8;
        if aot != 31 && freq_idx <= 12 {
            let chan = ((b1 >> 3) & 0x0F) as u8;
            let ch = if (1..=7).contains(&chan) {
                chan
            } else {
                chan_fallback
            };
            return Some((freq_idx, ch));
        }
        // escape-coded AOT / explicit 24-bit rate — exotic; fall through to codecpar
    }
    adts_freq_index((*acp).sample_rate).map(|fi| (fi, chan_fallback))
}

/// 7-byte ADTS header for a raw AAC frame of `payload_len` bytes. LG's Starfish decodes
/// ADTS-framed AAC (ss4s: aacInfo format="adts"); mp4/matroska carry RAW AAC, so we reframe.
/// Emits AAC-LC (ADTS profile 1 = object type 2 - 1); buffer fullness = VBR (0x7FF).
fn adts_header(freq_idx: u8, chan_cfg: u8, payload_len: usize) -> [u8; 7] {
    let frame_len = (payload_len + 7) as u32; // total incl. header, 13 bits
    [
        0xFF,
        0xF1, // sync(1111) | MPEG-4(0) | layer(00) | protection_absent(1)
        (1 << 6) | (freq_idx << 2) | (chan_cfg >> 2), // profile(01=LC) | freq_idx | chan hi bit
        ((chan_cfg & 3) << 6) | ((frame_len >> 11) as u8 & 0x03),
        ((frame_len >> 3) & 0xFF) as u8,
        (((frame_len & 0x07) as u8) << 5) | 0x1F, // frame_len lo 3 | fullness hi 5 (11111)
        0xFC,                                     // fullness lo 6 (111111) | num_blocks(00)
    ]
}

/// Whether a demux exit that published no access unit is a playback failure — the ONE rule both
/// the HLS and the progressive tails read. A demuxer that produced nothing and ended on its own
/// failed; one whose lane teardown aborted was told to stop (a reload, a seek, BACK), whatever
/// early exit that took, and must neither log a failure nor raise `demux_failed`.
fn unproductive_exit_failed(pushed_any: bool, lane_aborted: bool) -> bool {
    !pushed_any && !lane_aborted
}

/// The log line for an `avformat_open_input` that returned no context. Teardown aborts the lanes
/// and `read_cb` answers the abort with EOF, so an open cut short that way returns whatever the
/// truncated probe made of it (`AVERROR_INVALIDDATA` on a Matroska header) — a stop that must not
/// read as the source failing to open. Issue #266's normalize run was misread exactly so.
fn open_input_failure_note(r: c_int, lane_aborted: bool) -> String {
    if lane_aborted {
        format!("ff: aborted during open_input r={r}")
    } else {
        format!("ff: open_input failed r={r}")
    }
}

/// **How much further the VIDEO lane may run ahead while the AUDIO lane has nothing yet.**
///
/// The two lanes are filled from ONE read of the file, in file order, and each has its own byte cap
/// (`player::engine`'s `AQ_VIDEO_BYTES`, 10 MiB). A Matroska file whose audio track carries a
/// container `Delay` has no audio block before that many seconds of video, so the video lane can
/// fill before the first audio packet is even read. Measured on the device, the video lane held
/// 10,171,164 bytes when the first audio AU was read for the reported film (an E-AC-3 `Delay` of
/// 11.008 s at ~9.5 Mbit/s, after the pump had popped some) and 13,628,369 bytes for the
/// `pipe_late_audio_eac3` fixture (12 Mbit/s, the same delay). The demuxer parked on the full video
/// lane, the pipeline (which will not start its clock without audio) took no more video, and the
/// first audio block was never read: a start that buffered forever, while a resume past the delay
/// played at once. FFmpeg's own probe said as much on stderr: no E-AC-3 parameters within its 5 MB
/// `probesize`.
///
/// So from the first video push that WOULD PARK at the base cap while no audio AU has been pushed in
/// this open or seek, the video lane's cap is its base cap PLUS this much: **10 MiB + 24 MiB =
/// 34 MiB (~35.6 MB) in total.** The grant is lazy on purpose: an ordinary file (video a few
/// packets ahead of the first audio, B-frames, a muxer that leads with video) never fills the base
/// cap before its first audio packet, so it never sees the grant and keeps the memory behaviour it
/// always had; on a 60-100 Mbit/s UHD stream an eager grant would have cost 18-30 MiB of extra
/// queue on a RAM-tight 32-bit set for files that never needed it. That is a BOUND, not a
/// measurement of what a file needs: at an 11 s delay it holds video up to ~26 Mbit/s,
/// and a longer delay or a higher rate outruns it (23.8 s at 12 Mbit/s, 11.4 s at 25 Mbit/s). It is
/// large enough for the delays seen in the field and small enough for a 32-bit set that is
/// RAM-tight, and a file whose audio never comes cannot grow the lane without limit.
///
/// **Exhausting it fails the open** ([`PreAudioHeadroom::before_video`], [`VideoStep::Fail`]): the
/// lane is full, no audio has been read, and the consumer has popped nothing since the grant, so
/// parking would be a spinner nobody can leave. The demuxer says so in the log and raises the same
/// `demux_io_failed` the pump turns into a route recovery (an Auto Original falls back to HLS, which
/// the server remuxes with the delay applied) or the failure read-out. A lane the consumer IS
/// draining (a seek into an audioless stretch: the pump pops video while the demuxer reads at LAN
/// speed, and the engine's video-only prime escape plays it) is not that hang: the grant is
/// released and the lane parks like any full lane, as it did before the grant existed.
///
/// **The grant never outlives its epoch.** It is released when the audio lead-in has been pushed
/// (below), when the consumer is seen draining a full lane (above), at a seek (the next epoch
/// starts from the base cap) and when the read loop ends.
///
/// **The grant is not withdrawn when the first audio AU lands.** The engine feeds a late track's
/// first AU AND the next `AUDIO_LEAD_IN_NS` of audio ahead of a clock that cannot start without
/// them (`player::engine`), so the demuxer must READ that much audio with the video lane still full
/// and undrained. Withdrawing the headroom at the first audio push dropped the video cap under what
/// the lane held, the next video push parked, and the audio lane got two packets and no more (device
/// log: `feed a#1`, `feed a#2`, then nothing). The grant is therefore released once the audio lane
/// has been pushed [`AUDIO_LEAD_IN_NS`](crate::player::engine::AUDIO_LEAD_IN_NS) plus
/// [`LEAD_IN_MARGIN_NS`] of audio, and the release is `aq_release_headroom`: the cap follows the
/// consumer's drain down to the base rather than dropping under the lane's occupancy.
const PRE_AUDIO_VIDEO_HEADROOM: c_long = 24 * 1024 * 1024;

/// Audio pushed past the engine's lead-in before the video lane's headroom is released: the lead-in
/// is measured from the AU the engine ACCEPTS, in its own timeline, and the lane is pushed in whole
/// AUs, so the demuxer reads a little beyond it rather than exactly to it.
const LEAD_IN_MARGIN_NS: i64 = 500_000_000;

/// The per-open (and per-seek) state of the video lane's pre-audio headroom. The decisions
/// ([`before_video`](Self::before_video), [`after_audio`](Self::after_audio)) are PURE; the two
/// methods that take a lane ([`withdraw`](Self::withdraw), [`restart`](Self::restart)) only call
/// `aq_release_headroom`.
///
/// `granted` latches at the first video push that WOULD PARK at the base cap while an audio lane is
/// being fed and no audio has been read yet; a file whose audio comes first, or whose video never
/// fills the base cap before its audio, never gets a grant. `pops_at_grant` is the lane's pop
/// counter then, the evidence for "the consumer is draining". `first_audio_ns` is the pts of the
/// first audio AU pushed since the open or seek. `released` latches once the grant has been given
/// back, whichever way. Every field restarts on a seek ([`restart`](Self::restart)).
#[derive(Default, Debug)]
struct PreAudioHeadroom {
    granted: bool,
    released: bool,
    first_audio_ns: Option<i64>,
    pops_at_grant: u64,
}

/// What the video push about to be made means for the lane's headroom.
#[derive(Debug, PartialEq, Eq)]
enum VideoStep {
    /// Push as usual.
    Proceed,
    /// Raise the lane's cap by this much, then push: the base cap is full and no audio has come.
    Grant(c_long),
    /// The consumer is draining a lane that is full under the grant: give the grant back
    /// (`aq_release_headroom`) and push, parking like any full lane.
    Release,
    /// Full, no audio, and nothing popped since the grant: parking is a spinner nobody can leave.
    Fail,
}

impl PreAudioHeadroom {
    /// A video AU is about to be pushed. `audio_lane` is whether an audio lane is being fed at all,
    /// `would_park` whether the push would park at the cap now in force (`aq_would_park`), `pops`
    /// the lane's pop counter (`aq_pops`).
    fn before_video(&mut self, audio_lane: bool, would_park: bool, pops: u64) -> VideoStep {
        if !audio_lane || self.released || !would_park {
            return VideoStep::Proceed;
        }
        if !self.granted {
            if self.first_audio_ns.is_some() {
                // Audio came first: the ordinary backpressure applies.
                return VideoStep::Proceed;
            }
            self.granted = true;
            self.pops_at_grant = pops;
            return VideoStep::Grant(PRE_AUDIO_VIDEO_HEADROOM);
        }
        // The grant stands and the push would park: the cap it gave is spent.
        if pops != self.pops_at_grant {
            self.released = true;
            VideoStep::Release
        } else if self.first_audio_ns.is_none() {
            VideoStep::Fail
        } else {
            // Audio is in and the lead-in logic owns the release; nothing has been popped yet.
            VideoStep::Proceed
        }
    }

    /// An audio AU at `pts_ns` was pushed since the open or seek.
    fn after_audio(&mut self, pts_ns: i64) -> AudioStep {
        let first = self.first_audio_ns.is_none();
        let start = *self.first_audio_ns.get_or_insert(pts_ns);
        let release = self.granted
            && !self.released
            && pts_ns - start >= crate::player::engine::AUDIO_LEAD_IN_NS + LEAD_IN_MARGIN_NS;
        self.released |= release;
        AudioStep { first, release }
    }

    /// Give the lane's grant back if one stands: an exit from the granted state that no audio push
    /// or draining consumer reached (a seek, the end of the file, a stop, audio whose timestamps
    /// never span the lead-in). The cap follows the consumer down to the base.
    fn withdraw(&mut self, video_lane: *mut AuQueue) {
        if self.granted && !self.released {
            crate::aq::aq_release_headroom(video_lane);
            self.released = true;
        }
    }

    /// A seek starts a new epoch: whatever grant the last one left standing goes back, and every
    /// field starts over (the next video push that would park grants afresh).
    fn restart(&mut self, video_lane: *mut AuQueue) {
        self.withdraw(video_lane);
        *self = PreAudioHeadroom::default();
    }
}

/// What an audio push means for the video lane's headroom.
struct AudioStep {
    /// The first audio AU since the open or seek: the caller logs how far video had to run.
    first: bool,
    /// The audio lead-in is in: release the headroom now.
    release: bool,
}

#[cfg(test)]
mod pre_audio_headroom_tests {
    use super::*;

    const LEAD: i64 = crate::player::engine::AUDIO_LEAD_IN_NS + LEAD_IN_MARGIN_NS;

    const GRANT: VideoStep = VideoStep::Grant(PRE_AUDIO_VIDEO_HEADROOM);

    /// The grant is LAZY and made once: only at a video push that would park at the base cap with an
    /// audio lane being fed and no audio read. A push that fits (an ordinary file: video a few
    /// packets ahead of its audio), a file with no audio lane, or one whose audio came first never
    /// gets one.
    #[test]
    fn the_grant_is_made_once_and_only_when_video_would_park_ahead_of_the_audio() {
        let mut h = PreAudioHeadroom::default();
        assert_eq!(h.before_video(true, false, 0), VideoStep::Proceed, "room left: no grant");
        assert!(!h.granted);
        assert_eq!(h.before_video(true, true, 0), GRANT);
        assert_eq!(h.before_video(true, false, 0), VideoStep::Proceed, "the cap has room again");

        assert_eq!(
            PreAudioHeadroom::default().before_video(false, true, 0),
            VideoStep::Proceed,
            "no audio lane: nothing to wait for"
        );

        let mut audio_first = PreAudioHeadroom::default();
        assert!(audio_first.after_audio(0).first);
        assert_eq!(audio_first.before_video(true, true, 0), VideoStep::Proceed, "audio came first: no grant");
        assert!(!audio_first.after_audio(LEAD).release, "and so nothing to release");
    }

    /// The first audio AU does NOT release the grant: the engine needs `AUDIO_LEAD_IN_NS` of audio
    /// more, and the demuxer can only read it while the video lane still has room. The release
    /// comes exactly when that much audio, plus the margin, has been pushed.
    #[test]
    fn the_grant_is_held_until_the_audio_lead_in_has_been_pushed() {
        let mut h = PreAudioHeadroom::default();
        assert_eq!(h.before_video(true, true, 0), GRANT);
        let first = h.after_audio(11_008_000_000);
        assert!(first.first && !first.release, "the first AU alone releases nothing");
        assert!(!h.after_audio(11_008_000_000 + crate::player::engine::AUDIO_LEAD_IN_NS).release,
            "the engine's lead-in without the margin is not yet enough");
        let done = h.after_audio(11_008_000_000 + LEAD);
        assert!(!done.first && done.release, "lead-in plus margin pushed: release");
        assert!(!h.after_audio(11_008_000_000 + LEAD + 32_000_000).release, "released once");
    }

    /// A full lane under the grant with no audio and nothing popped since the grant is the hang
    /// the bound exists for; once audio is in, a full lane is ordinary backpressure (the lead-in
    /// logic owns what follows).
    #[test]
    fn a_full_lane_with_no_audio_and_no_pops_fails_the_open() {
        let mut h = PreAudioHeadroom::default();
        assert_eq!(h.before_video(true, true, 7), GRANT);
        assert_eq!(h.before_video(true, false, 7), VideoStep::Proceed, "room left");
        assert_eq!(h.before_video(true, true, 7), VideoStep::Fail, "full, no audio, nothing popped");
        h.after_audio(0);
        assert_eq!(h.before_video(true, true, 7), VideoStep::Proceed, "audio arrived: not this case");
    }

    /// **F1.** A lane whose consumer IS draining is not the hang: after a seek into an audioless
    /// stretch the pump pops video while the demuxer reads at LAN speed, and main parks that lane
    /// (and plays through the engine's video-only prime escape) instead of failing the open. The
    /// pop counter is read from a real lane.
    #[test]
    fn a_lane_that_is_draining_is_released_not_failed() {
        let mut q = crate::aq::aq_new(100);
        let qp = &mut *q as *mut AuQueue;
        let au = [0u8; 100];
        let push = || assert_eq!(crate::aq::aq_push(qp, au.as_ptr(), au.len() as c_int, 0, 1, 1), 0);
        let mut h = PreAudioHeadroom::default();
        for _ in 0..2 {
            push(); // 200 bytes held over the base cap of 100: a push would park
        }
        assert!(crate::aq::aq_would_park(qp));
        let VideoStep::Grant(extra) = h.before_video(true, crate::aq::aq_would_park(qp), crate::aq::aq_pops(qp)) else {
            panic!("a push that would park with no audio grants");
        };
        crate::aq::aq_set_headroom(qp, extra);
        // Fill past the standing cap: the next push would park again.
        while !crate::aq::aq_would_park(qp) {
            push();
        }
        assert_eq!(
            h.before_video(true, true, crate::aq::aq_pops(qp)),
            VideoStep::Fail,
            "full, no audio and no pops: the hang"
        );
        // The consumer pops, then the lane fills again.
        for _ in 0..3 {
            let n = crate::aq::aq_pop(qp, std::ptr::null_mut());
            assert!(!n.is_null());
            unsafe { libc::free(n as *mut c_void) };
        }
        while !crate::aq::aq_would_park(qp) {
            push();
        }
        assert_eq!(
            h.before_video(true, crate::aq::aq_would_park(qp), crate::aq::aq_pops(qp)),
            VideoStep::Release,
            "pops happened since the grant: the consumer is draining, so this is not the hang"
        );
        assert!(h.released);
        assert_eq!(h.before_video(true, true, crate::aq::aq_pops(qp)), VideoStep::Proceed, "released once");
    }

    /// **F3.** The seek reset must take the lane's grant away. Restarting the struct alone left a
    /// seek that landed while the grant was unreleased with the video lane at base + 24 MiB for the
    /// rest of the session (the next epoch may be audio-first, or carry audio whose pts never span
    /// the lead-in), with the exhaustion check off.
    #[test]
    fn a_seek_with_the_grant_outstanding_returns_the_lane_to_its_base_cap() {
        let mut q = crate::aq::aq_new(100);
        let qp = &mut *q as *mut AuQueue;
        let mut h = PreAudioHeadroom::default();
        let VideoStep::Grant(extra) = h.before_video(true, true, 0) else { panic!("grant") };
        crate::aq::aq_set_headroom(qp, extra);
        assert_eq!(crate::aq::aq_cap(qp), 100 + extra as i64);
        h.restart(qp);
        assert_eq!(crate::aq::aq_cap(qp), 100, "the lane is back at its base cap after the seek reset");
        assert_eq!(h.before_video(false, true, 0), VideoStep::Proceed, "and the state started over");
        assert!(!h.granted && !h.released && h.first_audio_ns.is_none());
        // The next epoch grants afresh.
        assert_eq!(h.before_video(true, true, 0), GRANT);
    }

    /// With the lane holding more than its base cap, the seek reset still never drops the cap under
    /// what the lane holds (`aq_release_headroom`), and a grant that was already released (or never
    /// made) is left alone.
    #[test]
    fn restart_releases_through_the_drain_and_leaves_settled_states_alone() {
        let mut q = crate::aq::aq_new(100);
        let qp = &mut *q as *mut AuQueue;
        let au = [0u8; 100];
        crate::aq::aq_set_headroom(qp, 1_000);
        for _ in 0..5 {
            assert_eq!(crate::aq::aq_push(qp, au.as_ptr(), au.len() as c_int, 0, 1, 1), 0);
        }
        let mut h = PreAudioHeadroom { granted: true, ..Default::default() };
        h.restart(qp);
        assert_eq!(crate::aq::aq_cap(qp), 500, "never under the lane's occupancy");
        // Never granted: no touch.
        crate::aq::aq_set_headroom(qp, 1_000);
        PreAudioHeadroom::default().restart(qp);
        assert_eq!(crate::aq::aq_cap(qp), 1_100, "nothing granted by this state, nothing withdrawn");
    }

    /// The geometry the constants claim, pinned: the cap is the base plus the headroom, an 11 s delay
    /// at the measured rates fits, and the fixture-sized read-ahead plus the engine's lead-in still
    /// fits, while a delay at ~35 Mbit/s does not (the bound is real).
    #[test]
    fn the_bound_holds_the_measured_cases_and_not_everything() {
        let base = crate::player::aq_caps().0;
        let cap = base + PRE_AUDIO_VIDEO_HEADROOM as i64;
        assert_eq!(cap, 34 * 1024 * 1024);
        let bytes = |mbit: f64, secs: f64| (mbit * 1e6 / 8.0 * secs) as i64;
        // the reported film: 11.008 s of ~9.5 Mbit/s video
        let film = bytes(9.495_770, 11.008);
        assert!(film > base, "the plain cap alone cannot hold it");
        assert!(film < cap);
        // the fixture: 12 Mbit/s, same delay, and then the lead-in's worth of video as well
        let lead_s = (crate::player::engine::AUDIO_LEAD_IN_NS + LEAD_IN_MARGIN_NS) as f64 / 1e9;
        assert!(bytes(12.0, 11.008 + lead_s) < cap, "headroom spans the pre-audio stretch AND the lead-in");
        assert!(bytes(35.0, 11.008) > cap, "a bound, not a promise");
    }
}

/// The demux thread body (spawned by `engine::start_bufferfeed`): read the file at `path` and push
/// its video and audio access units onto the two lanes.
pub fn demux(
    _off: &plx_base::task::OffFrame,
    path: std::path::PathBuf,
    acodec: String,
    aq: SendPtr<AuQueue>,
    aqa: SendPtr<AuQueue>,
) {
    PUSHED_ANY.store(false, Ordering::Relaxed);
    // Refuse rather than read a struct whose shape we do not know (see `boot`). EOF must still be
    // set on both lanes or `pump` waits forever on a queue nothing will ever fill — the same
    // contract the panic barrier below exists to keep.
    if !ABI_OK.load(Ordering::Relaxed) {
        crate::player::log("demux: refusing — FFmpeg ABI is not the one built for");
        crate::aq::aq_set_eof(aq.0);
        crate::aq::aq_set_eof(aqa.0);
        return;
    }
    DIAG_FIRST.store(true, Ordering::Relaxed);
    ensure_registered();
    let aq_p = aq.0; // VIDEO lane (also the AVIO abort ptr + EOF marker)
    let aqa_p = aqa.0; // AUDIO lane (es=2) — always a distinct queue on the ff (two-lane) path

    // PANIC BARRIER around the whole producer body. Not about the unwind itself — this thread is
    // started by `task::spawn`, so std already catches a panic at the thread boundary and turns it
    // into the `Err` that `task::join` logs. The problem is what the unwind SKIPS: the two
    // `aq_set_eof` calls in the tail below. The consumer is a one-way FIFO with no other liveness
    // signal — `aq_pop` reports EOF only from the `eof` flag the producer sets — so a producer that
    // dies without setting it leaves `pump` feeding a queue that will never fill and never end:
    // no EOS is ever pushed (`engine.rs`'s EOS is keyed on the video lane's true EOF), no error is
    // ever surfaced, and the app sits on a frozen picture until BACK. That is strictly worse than
    // the crash it came from — a rare panic became a common-looking hang with no log line pointing
    // at it. So: catch it, name it in the event log, and then run the SAME teardown the normal exit
    // runs, so the pump terminates the way it does at end-of-stream.
    //
    // `AssertUnwindSafe` is honest here rather than a rubber stamp: the state the closure mutates
    // across the boundary is the two AU queues (their own pthread mutexes, and this is their only
    // producer, which is now dead) and `SHARED`'s atomics. What a panic DOES leak is the FFmpeg
    // side — `AVFormatContext`/`AVIOContext`/`AVPacket` are freed at the end of the block, not by a
    // `Drop`, so an unwind past them leaks them plus the socket buffer. That is deliberate: the
    // alternative is calling `avformat_close_input` on a context whose invariants a panicking
    // libavformat may have left broken, and a one-off leak on a path that is meant never to run is
    // cheaper than a segfault. The session ends here either way.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Run-once breakable block, NOT a retry loop: every `break` below is an early exit to the
        // EOF/cleanup tail after it. An audio-track switch replaces the engine outright, which
        // spawns a new demux thread.
        loop {
            unsafe {
                // Teardown may have raced us here (it aborts the lanes, then JOINS this thread on
                // the main thread): refuse to start reading for it.
                if crate::aq::aq_is_aborted(aq_p) {
                    crate::player::log("ff: aborted before open");
                    break;
                }
                let file = match crate::localfile::LocalFile::open(&path) {
                    Ok(f) => f,
                    Err(e) => {
                        crate::player::log(&format!("ff: local open FAILED: {e}"));
                        SHARED.demux_failed.store(true, Ordering::Release);
                        break;
                    }
                };
                let size = file.size();
                SHARED.file_size.store(size, Ordering::Release);
                crate::player::log(&format!("ff: open local size={size}"));
                let mut state = Box::new(AvioState {
                    file: Box::new(file),
                    aq: aq_p,
                    off: 0,
                    size,
                    io_failed: false,
                });
                let buf = av_malloc(65536) as *mut u8;
                if buf.is_null() {
                    crate::player::log("ff: av_malloc failed");
                    break;
                }
                let avio = avio_alloc_context(
                    buf,
                    65536,
                    0,
                    &mut *state as *mut AvioState as *mut c_void,
                    Some(read_cb),
                    None,
                    Some(seek_cb),
                );
                if avio.is_null() {
                    crate::player::log("ff: avio_alloc_context failed");
                    free_ptr(buf as *mut c_void);
                    break;
                }
                let mut fmt = avformat_alloc_context();
                if fmt.is_null() {
                    crate::player::log("ff: avformat_alloc_context failed");
                    free_avio(avio);
                    break;
                }
                (*fmt).pb = avio;
                let r = avformat_open_input(
                    &mut fmt,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                if r < 0 || fmt.is_null() {
                    crate::player::log(&open_input_failure_note(r, crate::aq::aq_is_aborted(aq_p)));
                    free_avio(avio);
                    break;
                }
                if avformat_find_stream_info(fmt, std::ptr::null_mut()) < 0 {
                    crate::player::log("ff: find_stream_info failed");
                    avformat_close_input(&mut fmt);
                    free_avio(avio);
                    break;
                }
                // The container's own track names, published before anything else can fail: the track
                // menu is the only reader and it wants them whether or not this part turns out to have
                // a video stream. See `stream_name` for why they are read at all — for an MP4 they are
                // the ONLY place a track's identity exists, PMS having dropped it.
                {
                    let (audio, subs) = track_names(fmt);
                    let named = audio
                        .iter()
                        .chain(subs.iter())
                        .filter(|n| !n.is_empty())
                        .count();
                    if named > 0 {
                        crate::player::log(&format!(
                            "ff: container names {named}/{} tracks (a={} s={})",
                            audio.len() + subs.len(),
                            audio.len(),
                            subs.len()
                        ));
                    }
                    *SHARED.track_names.lock().unwrap() = crate::player::TrackNames { audio, subs };
                }
                let vi =
                    av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, -1, -1, std::ptr::null_mut(), 0);
                // native audio-track selection: feed the chosen Nth audio stream (SHARED.desired_
                // audio_idx, set by the track menu), else the pipeline's best/default audio.
                let want_aidx = SHARED.desired_audio_idx.load(Ordering::Relaxed);
                let ai = if want_aidx >= 0 {
                    nth_audio_stream(fmt, want_aidx).unwrap_or_else(|| {
                        av_find_best_stream(
                            fmt,
                            AVMEDIA_TYPE_AUDIO,
                            -1,
                            -1,
                            std::ptr::null_mut(),
                            0,
                        )
                    })
                } else {
                    // default: feed the track matching the Load payload's codec (Media[0].audioCodec),
                    // NOT av_find_best_stream — a codec mismatch stalls the audio ES and wedges video.
                    audio_stream_matching(fmt, &acodec).unwrap_or_else(|| {
                        av_find_best_stream(
                            fmt,
                            AVMEDIA_TYPE_AUDIO,
                            -1,
                            -1,
                            std::ptr::null_mut(),
                            0,
                        )
                    })
                };
                if vi < 0 {
                    // "No video stream" alone reads as a demuxer fault. When audio IS present the
                    // demuxer worked fine — the stream itself arrived without video, which on a
                    // transcode URL means the server dropped the track (no usable video target —
                    // issue #22: an HEVC-only target on a server without Plex Pass). Record which
                    // shape this is so the error the user sees can say so; the main thread words it.
                    if ai >= 0 {
                        SHARED.demux_no_video.store(true, Ordering::Release);
                        crate::player::log("ff: no video stream — the stream carries audio only");
                    } else {
                        crate::player::log("ff: no video stream");
                    }
                    avformat_close_input(&mut fmt);
                    free_avio(avio);
                    break;
                }
                let streams = (*fmt).streams;
                let vst = *streams.add(vi as usize);
                let vcp = stream_codecpar(vst);
                // AAC needs ADTS framing for LG's decoder (mp4/mkv carry raw AAC). Precompute the
                // per-frame ADTS fields (freq index + channel config) for the selected audio stream;
                // None => not AAC (or a non-standard rate) => fed verbatim.
                let aac_adts: Option<(u8, u8)> = if ai >= 0 {
                    let acp = stream_codecpar(*streams.add(ai as usize));
                    if (*acp).codec_id == AV_CODEC_ID_AAC {
                        adts_params(acp)
                    } else {
                        None
                    }
                } else {
                    None
                };
                let is_dts = ai >= 0 && std::ffi::CStr::from_ptr(avcodec_get_name(
                    (*stream_codecpar(*streams.add(ai as usize))).codec_id
                )).to_bytes() == b"dts";
                // The television's decoder takes the DTS core only, so a DTS-HD packet is cut
                // down to it. The Linux build's GStreamer decodes the whole stream (FFmpeg's DCA
                // decoder reads the XLL lossless extension), so there it is fed as it is: a DTS-HD
                // MA track plays lossless, at its own channel count.
                let dts_audio = is_dts && !true;
                if is_dts {
                    let profile = dts_profile_name((*stream_codecpar(*streams.add(ai as usize))).profile);
                    crate::player::log(&if dts_audio {
                        format!("ff: {profile}: DTS core extraction on (DTS-HD extensions discarded)")
                    } else {
                        format!("ff: {profile}: fed whole (extensions decoded by the sink)")
                    });
                }
                if let Some((fi, ch)) = aac_adts {
                    crate::player::log(&format!(
                        "ff: AAC → ADTS reframing on (freq_idx={fi} ch={ch})"
                    ));
                }
                // The GStreamer sink describes FLAC, Opus, Vorbis and PCM from the stream's own
                // parameters rather than a codec name; hand them over before any audio is pushed.
                if ai >= 0 {
                    let acp = stream_codecpar(*streams.add(ai as usize));
                    let (ed, n) = ((*acp).extradata, (*acp).extradata_size);
                    crate::player::gst_sink::note_audio(crate::player::gst_sink::AudioParams {
                        codec: std::ffi::CStr::from_ptr(avcodec_get_name((*acp).codec_id)).to_string_lossy().into_owned(),
                        extradata: if ed.is_null() || n <= 0 { Vec::new() } else { std::slice::from_raw_parts(ed, n as usize).to_vec() },
                        rate: (*acp).sample_rate,
                        channels: (*acp).ch_layout.nb_channels,
                    });
                }
                let dur = fmt_duration(fmt);
                if dur > 0 {
                    SHARED
                        .duration_ns
                        .store(dur.saturating_mul(1000), Ordering::Relaxed);
                }
                // The NAME as well as the id, and the name is what anything downstream should grade.
                // A raw AV_CODEC_ID is an FFmpeg enum, and that enum RENUMBERS between majors — H264
                // is 28 on the n3.3 the televisions ship, 27 on FFmpeg 6, and HEVC is 174 / 173 / 172
                // across n3.3 / 6 / 9. The on-device suite asserted the bare number, so bundling our
                // own FFmpeg failed all 21 cases at once on a codec the app had identified perfectly
                // well. `avcodec_get_name` is stable, means what the assertion meant, and is free.
                // Publish the coded size: the webOS 5+ exported window needs the frame it is being
                // fed, and this is the only place that knows it for certain — a transcode's declared
                // dimensions and its actual output need not agree.
                SHARED.publish_video_raster((*vcp).width, (*vcp).height);
                let cname =
                    std::ffi::CStr::from_ptr(avcodec_get_name((*vcp).codec_id)).to_string_lossy();
                // Refuse a stream the pipeline cannot be fed, BEFORE anything else is opened for it
                // (subtitle decoders, ASS sources): the exit then has the same shape as the "no video
                // stream" one above, with nothing to release but the input and its AVIO.
                let Some(framing) = VideoFraming::of_codec_id((*vcp).codec_id) else {
                    crate::player::log(&format!(
                        "ff: no framing for video codec {cname} (codec_id={}); refusing the stream",
                        (*vcp).codec_id
                    ));
                    SHARED.demux_failed.store(true, Ordering::Release);
                    avformat_close_input(&mut fmt);
                    free_avio(avio);
                    break;
                };
                // What the Load declared (`dg_load_v`, 0 when this start sent a static payload) must
                // be what the file is: a Load that says one codec over another codec's bitstream is a
                // black screen, so stop and name both.
                let declared = SHARED.dg_load_v.load(Ordering::Relaxed);
                if declared != 0 && declared != framing.load_video().diag_code() {
                    crate::player::log(&format!(
                        "ff: the Load declared {} but the file is {cname}; refusing the stream",
                        ["?", "H264", "H265", "AV1", "VP9"].get(declared as usize).copied().unwrap_or("?")
                    ));
                    SHARED.demux_failed.store(true, Ordering::Release);
                    avformat_close_input(&mut fmt);
                    free_avio(avio);
                    break;
                }
                // WHAT THE STREAM ACTUALLY IS, not just what it is called. `codec=hevc` is the same
                // four letters for an SDR file, an HDR10 file, a Dolby Vision Profile 8.1 file whose
                // base layer is that same HDR10, and a Profile 5 file that will display in visibly
                // wrong colours — `avcodec_get_name` cannot tell them apart and neither could this log
                // line or any assertion built on it. These fields can. `trc`/`pri`/`spc` are the raw
                // AVCOL_* enum values, logged as NUMBERS because naming them would mean binding three
                // more FFmpeg symbols for a diagnostic: trc 16 = smpte2084 (PQ/HDR10), 18 = arib-std-b67
                // (HLG), spc 9 = bt2020nc, pri 9 = bt2020, and **2 = UNSPECIFIED on all three** (every
                // one of those six numbers checked against the vendored `libavutil/pixfmt.h`, not
                // recalled). A Profile 5 file is expected to read 2/2/2, since IPT-PQ signals no
                // ordinary transfer at all — inferred rather than seen from here, but from the same
                // probe: PMS reports NO `colorTrc`, `colorSpace` or `colorPrimaries` at all on the dev
                // server's P5 item while sending all three on every P8 (swept 2026-08-21). This line
                // is where a television settles it.
                // All three were declared in the offsets table above and read NOWHERE until now.
                let dv = match dovi_conf(vcp) {
                    // `bl_compat` is the field that decides whether the base layer means anything on
                    // its own, so it is logged beside the profile rather than left to be inferred
                    // from it — 0 is Profile 5's "none", 1 the HDR10 of a Profile 8.1.
                    Some(d) => format!(
                        " dovi=P{} level={} bl_compat={} rpu={} el={} bl={}",
                        d.dv_profile,
                        d.dv_level,
                        d.dv_bl_signal_compatibility_id,
                        d.rpu_present_flag,
                        d.el_present_flag,
                        d.bl_present_flag
                    ),
                    None => String::new(),
                };
                crate::player::log(&format!(
                    "ff: v=#{vi} codec={cname} codec_id={} {}x{} trc={} pri={} spc={}{dv} a=#{ai} dur_ns={}",
                    (*vcp).codec_id,
                    (*vcp).width,
                    (*vcp).height,
                    (*vcp).color_trc,
                    (*vcp).color_primaries,
                    (*vcp).color_space,
                    SHARED.duration_ns.load(Ordering::Relaxed)
                ));

                // Client-rendered subtitles (direct-play only): enumerate subtitle streams in FILE
                // order so their 0-based position maps 1:1 to the track menu's desired_sub_idx (which
                // indexes metadata d.subs, i.e. every streamType==3 stream in document order). The
                // selected track is read LIVE in the loop below, so switching subtitles mid-play takes
                // effect with no reopen (parity with mkv.rs's active_sub_track).
                let mut sub_tracks = SubTracks::open(fmt, streams);
                let ass_generation =
                    begin_ass_sources(&crate::localfile::file_url(&path), fmt, streams, sub_tracks.entries());
                sub_tracks.log_tracks();

                // Hand-roll length-prefix -> Annex-B + prepend VPS/SPS/PPS at every keyframe (the
                // format Starfish decodes). FFmpeg's hevc_mp4toannexb does NOT reliably prepend the
                // parameter sets on this 3.3 build (it leaves the keyframe starting with SEI), so we
                // build the AU from the codecpar extradata; libavformat still owns demux + seeking.
                // AV1 is framed by av1_packet_to_tu as Section-5 temporal units with the av1C config
                // in-band on keyframes, because the TV pipeline is given no codec_data for AV1.
                let is_hevc = framing == VideoFraming::AnnexB { hevc: true };
                let is_av1 = framing == VideoFraming::Av1;
                let is_vp9 = framing == VideoFraming::Vp9;
                let (param_blob, nal_len_size) = if is_av1 || is_vp9 {
                    (Vec::new(), 0)
                } else {
                    parse_extradata(
                        (*vcp).extradata,
                        (*vcp).extradata_size.max(0) as usize,
                        is_hevc,
                    )
                };
                let av1_ed: &[u8] = if (*vcp).extradata.is_null() || (*vcp).extradata_size <= 0 {
                    &[]
                } else {
                    std::slice::from_raw_parts((*vcp).extradata, (*vcp).extradata_size as usize)
                };
                let av1_cfg = if is_av1 { parse_av1c(av1_ed) } else { Vec::new() };
                let av1_framing = if is_av1 {
                    Av1Framing::from_trigger(plx_base::devtrig::read("av1obu").as_deref())
                } else {
                    Av1Framing::DEFAULT
                };
                #[cfg(feature = "devtriggers")]
                {
                    if is_av1 && av1_framing != Av1Framing::DEFAULT {
                        crate::player::log(&format!(
                            "ff: av1obu trigger: td={} seq={} raw={}",
                            av1_framing.td, av1_framing.seq, av1_framing.raw
                        ));
                    }
                }
                // A VP9 stream's PQ is in the container only; hand it to the pump, which writes it into
                // the sourceInfo envelope ACB is sent. Always replaced, so a previous stream's colour
                // cannot outlive it.
                *SHARED.container_hdr.lock().unwrap() = if is_vp9 { Some(container_hdr(vcp)) } else { None };
                crate::player::log(&format!(
                    "ff: param_sets={} bytes nal_len={} is_hevc={} is_av1={} is_vp9={} av1_cfg={}B",
                    param_blob.len(),
                    nal_len_size,
                    is_hevc,
                    is_av1,
                    is_vp9,
                    av1_cfg.len()
                ));
                let mut aubuf: Vec<u8> = Vec::with_capacity(4 * 1024 * 1024);

                let pkt = av_packet_alloc();
                if pkt.is_null() {
                    crate::player::log("ff: packet_alloc failed");
                    avformat_close_input(&mut fmt);
                    free_avio(avio);
                    break;
                }

                // The video lane's pre-audio headroom (`PreAudioHeadroom`): granted at the first
                // video push that WOULD PARK at the base cap with no audio yet, held until the
                // engine's audio lead-in has been pushed, then released as the consumer drains.
                // Restarted (and the lane's grant withdrawn) by every seek and at loop exit.
                let mut headroom = PreAudioHeadroom::default();
                SHARED.first_audio_pts.store(i64::MIN, Ordering::Release);
                // INNER read loop
                loop {
                    // Direct-play seek (and the armed resume, which is just a seek published before
                    // the first read): the pump leaves a target ns in SHARED.seek_to_ns and THIS
                    // thread — the only one that ever touches `fmt` — av_seek_frame's between two
                    // av_read_frame calls. A transcode leaves seek_to_ns=-1 (start.mkv is already
                    // 0-based at &offset) and so skips this.
                    //
                    // It used to be an INTERRUPT instead: the pump shutdown(2)'d the socket to break
                    // the read, and the outer loop reopened the URL and seeked the fresh context.
                    // That could not work, and the test suite hid it behind inherited resume offsets
                    // until 2026-07-28. Our AVIO is SEEKABLE (`seek_cb` reopens with a byte Range),
                    // so libavformat treats a read error as recoverable, calls seek_cb, and gets a
                    // brand-new connection at the same offset — av_read_frame never returned an
                    // error, the inner loop never broke, and no reopen ever happened. Every
                    // direct-play seek ran out the stuck-watchdog on PRE-seek packets
                    // (`rebase: dropping stale kf` at the old position) and escalated to a full
                    // reload. Seeking here needs no interrupt at all, so nothing can race it.
                    let seek_ns = SHARED.seek_to_ns.swap(-1, Ordering::Acquire);
                    if seek_ns >= 0 {
                        let ts = av_rescale_q(seek_ns, NS_TB, stream_time_base(vst));
                        // Fence completed renders from the old position before new packets arrive.
                        crate::player::ass_source::seek(ass_generation);
                        let sr = av_seek_frame(fmt, vi, ts, AVSEEK_FLAG_BACKWARD);
                        crate::player::log(&format!(
                            "ff: seek {}s rv={sr}",
                            seek_ns / 1_000_000_000
                        ));
                        headroom.restart(aq_p);
                        SHARED.first_audio_pts.store(i64::MIN, Ordering::Release);
                    }
                    let r = av_read_frame(fmt, pkt);
                    if frame_read_failed(&mut state, r) {
                        // Genuine end of stream, teardown, or an unrecovered I/O error published by
                        // `frame_read_failed`. NOT an app seek — that is serviced at the top of this
                        // loop and never surfaces as a read error.
                        break;
                    }
                    let si = (*pkt).stream_index;
                    if si == vi {
                        let is_key = if is_av1 {
                            let pkt_bytes: &[u8] = if (*pkt).data.is_null() {
                                &[]
                            } else {
                                std::slice::from_raw_parts((*pkt).data, (*pkt).size.max(0) as usize)
                            };
                            av1_packet_to_tu(
                                pkt_bytes,
                                (*pkt).flags & AV_PKT_FLAG_KEY != 0,
                                &av1_cfg,
                                av1_framing,
                                &mut aubuf,
                            )
                        } else if is_vp9 {
                            let pkt_bytes: &[u8] = if (*pkt).data.is_null() {
                                &[]
                            } else {
                                std::slice::from_raw_parts((*pkt).data, (*pkt).size.max(0) as usize)
                            };
                            vp9_packet_to_frame(pkt_bytes, (*pkt).flags & AV_PKT_FLAG_KEY != 0, &mut aubuf)
                        } else {
                            packet_to_annexb(
                                (*pkt).data,
                                (*pkt).size.max(0) as usize,
                                nal_len_size,
                                is_hevc,
                                &param_blob,
                                &mut aubuf,
                            )
                        };
                        let pts = pts_ns(pkt, vst);
                        if DIAG_FIRST.swap(false, Ordering::Relaxed) {
                            let n = aubuf.len().min(40);
                            let head: String = aubuf[..n]
                                .iter()
                                .map(|b| format!("{:02x}", b))
                                .collect::<Vec<_>>()
                                .join(" ");
                            crate::player::log(&format!(
                                "ff: AU#0 size={} key={} head=[{}]",
                                aubuf.len(),
                                is_key,
                                head
                            ));
                        }
                        match headroom.before_video(
                            ai >= 0 && FEED_AUDIO.load(Ordering::Relaxed),
                            crate::aq::aq_would_park(aq_p),
                            crate::aq::aq_pops(aq_p),
                        ) {
                            VideoStep::Proceed => {}
                            VideoStep::Grant(extra) => crate::aq::aq_set_headroom(aq_p, extra),
                            VideoStep::Release => {
                                // The lane is full under the grant but the consumer is draining it
                                // (a seek into an audioless stretch): not the hang the grant is
                                // bounded for. Park like any full lane, as before the grant.
                                crate::aq::aq_release_headroom(aq_p);
                                crate::player::log(&format!(
                                    "ff: video headroom released with {} bytes queued: the lane is draining and no audio has come",
                                    crate::aq::aq_bytes(aq_p)
                                ));
                            }
                            VideoStep::Fail => {
                                // The grant is spent, no audio AU has been read, and nothing has
                                // been popped since it was given: the consumer cannot drain video
                                // before its clock starts (which needs audio), so parking here is
                                // a spinner nobody can leave. Fail the open instead, the way an
                                // unfeedable audio track does, so the route recovers (Auto
                                // Original -> HLS) or the failure read-out shows.
                                crate::player::log(&format!(
                                    "ff: no audio AU after {} bytes of video queued (cap {} = base + the {} byte pre-audio headroom); failing the open",
                                    crate::aq::aq_bytes(aq_p),
                                    crate::aq::aq_cap(aq_p),
                                    PRE_AUDIO_VIDEO_HEADROOM
                                ));
                                SHARED.demux_failed.store(true, Ordering::Release);
                                SHARED.demux_io_failed.store(true, Ordering::Release);
                                av_packet_unref(pkt);
                                break;
                            }
                        }
                        let pushed = crate::aq::aq_push(
                            aq_p,
                            aubuf.as_ptr(),
                            aubuf.len() as c_int,
                            pts,
                            if is_key { 1 } else { 0 },
                            1);
                        av_packet_unref(pkt);
                        if pushed != 0 {
                            break;
                        }
                        PUSHED_ANY.store(true, Ordering::Relaxed);
                    } else if si == ai && FEED_AUDIO.load(Ordering::Relaxed) {
                        let ast = *streams.add(ai as usize);
                        let pts = pts_ns(pkt, ast);
                        if headroom.first_audio_ns.is_none() {
                            // Before the push, so the feed thread that pops this AU sees it: the
                            // audio lane feeds a late-starting track's first AU on this mark.
                            SHARED.first_audio_pts.store(pts, Ordering::Release);
                        }
                        let pushed = if let Some((freq_idx, chan_cfg)) = aac_adts {
                            // prepend a 7-byte ADTS header so LG's decoder can frame the raw AAC
                            let plen = (*pkt).size as usize;
                            let mut framed = Vec::with_capacity(7 + plen);
                            framed.extend_from_slice(&adts_header(freq_idx, chan_cfg, plen));
                            framed.extend_from_slice(std::slice::from_raw_parts((*pkt).data, plen));
                            crate::aq::aq_push(
                                aqa_p,
                                framed.as_ptr(),
                                framed.len() as c_int,
                                pts,
                                1,
                                2)
                        } else if dts_audio {
                            let raw = std::slice::from_raw_parts((*pkt).data, (*pkt).size.max(0) as usize);
                            let Some(core) = dts_core_packet(raw) else {
                                crate::player::log("ff: DTS packet has no complete supported core; refusing audio feed");
                                SHARED.demux_failed.store(true, Ordering::Release);
                                SHARED.demux_io_failed.store(true, Ordering::Release);
                                av_packet_unref(pkt);
                                break;
                            };
                            crate::aq::aq_push(
                                aqa_p, core.as_ptr(), core.len() as c_int, pts, 1, 2)
                        } else {
                            crate::aq::aq_push(
                                aqa_p,
                                (*pkt).data,
                                (*pkt).size,
                                pts,
                                1,
                                2)
                            // AUDIO lane
                        };
                        av_packet_unref(pkt);
                        if pushed != 0 {
                            break;
                        }
                        let step = headroom.after_audio(pts);
                        if step.first && headroom.granted {
                            // The late audio arrived: say how far ahead video had to run.
                            crate::player::log(&format!(
                                "ff: first audio at {}ms after {} bytes of video queued ahead of it",
                                pts / 1_000_000,
                                crate::aq::aq_bytes(aq_p)
                            ));
                        }
                        if step.release {
                            // The engine's lead-in is on the audio lane: from here the video lane
                            // falls back to its base cap as the consumer drains it, never under what
                            // it holds (see `PRE_AUDIO_VIDEO_HEADROOM`).
                            crate::aq::aq_release_headroom(aq_p);
                            crate::player::log(&format!(
                                "ff: audio lead-in pushed ({}ms past the first); video headroom released with {} bytes queued",
                                (pts - headroom.first_audio_ns.unwrap_or(pts)) / 1_000_000,
                                crate::aq::aq_bytes(aq_p)
                            ));
                        }
                    } else if let Some(sub_pos) = sub_tracks.position(si) {
                        // Subtitle packet: `SubTracks::decode` yields a cue for EVERY text track
                        // (not just the selected one) and decodes every image track while subtitles
                        // are on; see ff_subs.rs. Subtitles carry no ES -> never fed to the pipeline.
                        if let Some(cue) = sub_tracks.decode(sub_pos, pkt, *streams.add(si as usize)) {
                            cue.push(ass_generation);
                        }
                        av_packet_unref(pkt);
                    } else {
                        av_packet_unref(pkt);
                    }
                    if crate::aq::aq_is_aborted(aq_p) {
                        break;
                    }
                }

                // Whatever way the read loop ended, the video lane does not keep a standing grant.
                headroom.withdraw(aq_p);

                // cleanup this stream (we own pb, so close_input won't free the AVIO)
                sub_tracks.close(); // reopened fresh on the next outer pass
                let mut pkt_m = pkt;
                av_packet_free(&mut pkt_m);
                avformat_close_input(&mut fmt);
                free_avio(avio);
                let _ = &state; // keep the AvioState alive until after free_avio
            }

            if unsafe { crate::aq::aq_is_aborted(aq_p) } {
                break;
            }
            break;
        }
    })); // end panic barrier. The body above is deliberately NOT re-indented into the closure: a
    // ~340-line whitespace diff would bury every real change this file ever gets again.

    // The panic path joins the normal one HERE, so both leave the consumer in the same state.
    if let Err(e) = &outcome {
        // The payload is almost always a `&str`/`String` (a slice index, an `unwrap`), and the
        // message is worth far more than the bare fact of a panic: this thread walks packet buffers
        // whose sizes come off the wire, so knowing WHICH one gave way is most of the triage. The
        // C tracer in `main.c` cannot help here — nothing faults, so there is no PC to symbolize.
        let what = e
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| e.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string payload>".to_string());
        crate::player::log(&format!("ff: demux PANICKED — {what}"));
        // The producer is gone before it could publish a duration or a single frame, which is the
        // one case EOF alone cannot resolve: `engine.rs` pushes EOS only once the pipeline has
        // reached `Stage::Streaming`, so a panic during open/find_stream_info would otherwise leave
        // the pump waiting on a stage it can never reach. This is the same flag the `http_open`
        // failure above raises, and `pump` turns it into `PlaybackState::Error` (gated on
        // `frames == 0`, so a panic MID-playback is left to the EOF/EOS path below instead).
        SHARED.demux_failed.store(true, Ordering::Release);
    }
    // A demux that ends having produced NOTHING is a failure, whatever route it took out.
    // Only two exits used to say so — the http_open failure and the panic handler — while every
    // other early `break` in the open sequence (the ABI refusal, av_malloc, avio_alloc_context,
    // avformat_alloc_context, open_input, find_stream_info, no video stream) simply set EOF and
    // returned. EOF with zero AUs is indistinguishable from a zero-length file: the pump keeps
    // waiting, `frames` stays 0, and the HUD says "Buffering…" forever with nothing in the log to
    // say why. That is precisely the report that came back from webOS 6 and 10, and it is why the
    // report could not name a cause. Failing loudly here does not fix any of those bugs; it makes
    // the next one diagnosable. An exit teardown asked for is not one of them: see
    // `unproductive_exit_failed`.
    if unproductive_exit_failed(PUSHED_ANY.load(Ordering::Relaxed), unsafe {
        crate::aq::aq_is_aborted(aq_p)
    }) {
        crate::player::log("ff: demux produced no access units — treating as a failure");
        SHARED.demux_failed.store(true, Ordering::Release);
    }
    crate::aq::aq_set_eof(aq_p);
    crate::aq::aq_set_eof(aqa_p); // EOF on the audio lane too
    crate::player::log("ff: demux ended");
}

// ---------------------------------------------------------------------------------------
#[cfg(test)]
#[path = "ff_test_support.rs"]
mod test_support;

#[cfg(test)]
#[path = "ff_packet_framing_tests.rs"]
mod packet_framing_tests;

#[cfg(test)]
#[path = "ff_image_subtitle_tests.rs"]
mod image_subtitle_tests;

#[cfg(test)]
#[path = "ff_late_audio_tests.rs"]
mod late_audio_tests;
