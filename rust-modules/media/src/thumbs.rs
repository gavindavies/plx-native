//! **Scrub preview frames** (`PiTV Redesign.dc.html` 1d): the picture at the time the viewer is
//! scrubbing to, in a 384 x 216 card above the playhead.
//!
//! A local file only — its frames are a seek away. One worker per file holds a paused GStreamer
//! `playbin` (video only) whose sink scales to [`W`] x [`H`] RGBA, letterboxed to keep the
//! picture's own aspect. A request names a time; the worker serves the NEWEST one and drops the
//! rest, because a viewer holding RIGHT asks for dozens a second and wants only where the bar is
//! now. It seeks to the nearest keyframe — a preview's picture, not a frame-exact one, which is
//! the cheap seek and the one trick-play thumbnails are made from everywhere else — and pulls the
//! preroll frame.
//!
//! A strip of stills (the Chapters strip's) asks for all of its frames at once
//! ([`Thumbnailer::prefetch`]); they are made in order behind any scrub.
//!
//! Frames are kept by [`BUCKET_NS`] so a bar that wanders back over ground it has covered shows
//! a picture at once; the cache is bounded ([`KEEP`]) and the oldest frames go first. Nothing here
//! touches GL: the player screen uploads the frame it shows (`screens::player`).
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// The frame size: the preview card's.
pub const W: u32 = 384;
pub const H: u32 = 216;
/// Frames are kept per this much of the timeline.
pub const BUCKET_NS: i64 = 2_000_000_000;
/// The most frames kept (~330 KB each).
const KEEP: usize = 48;
/// How long a state change or a seek may take before the worker gives up on the file.
const WAIT: Duration = Duration::from_secs(5);

/// One decoded frame: its bucket and its RGBA pixels, `W * H * 4` bytes.
pub struct Frame {
    pub bucket: i64,
    pub rgba: Vec<u8>,
}

#[derive(Default)]
struct State {
    /// The newest time asked for, not yet served.
    wanted: Option<i64>,
    /// Times asked for ahead of need ([`Thumbnailer::prefetch`]), served in order once no
    /// `wanted` is waiting.
    backlog: VecDeque<i64>,
    /// Frames, oldest first.
    frames: VecDeque<Arc<Frame>>,
    /// The file could not be opened or decoded: ask no more.
    failed: bool,
    /// The owner is gone: the worker ends.
    closed: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

/// The preview frames of one file. Dropping it ends its worker.
pub struct Thumbnailer {
    path: PathBuf,
    shared: Arc<Shared>,
}

impl Thumbnailer {
    /// Start a worker for the file at `path`. `None` when the thread cannot be had.
    pub fn new(path: &Path) -> Option<Self> {
        let shared = Arc::new(Shared { state: Mutex::new(State::default()), wake: Condvar::new() });
        let (worker, file) = (shared.clone(), path.to_path_buf());
        let spawned = plx_base::task::spawn_small("thumbs", move || run(&file, &worker));
        spawned.then(|| Self { path: path.to_path_buf(), shared })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Ask for the frame at `ns`. Cheap, and coalesced: only the newest request is served.
    pub fn request(&self, ns: i64) {
        let bucket = bucket_of(ns);
        let mut st = lock(&self.shared);
        if st.failed || st.frames.iter().any(|f| f.bucket == bucket) {
            return;
        }
        st.wanted = Some(ns.max(0));
        self.shared.wake.notify_one();
    }

    /// Ask for the frames at each of `times`, in order, behind any scrub request — a strip of
    /// chapter stills, which wants every one rather than only the newest.
    pub fn prefetch(&self, times: &[i64]) {
        let mut st = lock(&self.shared);
        if st.failed {
            return;
        }
        for &ns in times {
            let bucket = bucket_of(ns);
            if !st.frames.iter().any(|f| f.bucket == bucket) && !st.backlog.iter().any(|&b| bucket_of(b) == bucket) {
                st.backlog.push_back(ns.max(0));
            }
        }
        self.shared.wake.notify_one();
    }

    /// The frame of `ns`'s own bucket, `None` until it is decoded.
    pub fn frame_exact(&self, ns: i64) -> Option<Arc<Frame>> {
        let bucket = bucket_of(ns);
        lock(&self.shared).frames.iter().find(|f| f.bucket == bucket).cloned()
    }

    /// The best frame to show for `ns`: its own bucket's when that is decoded, else the frame
    /// decoded last (the bar's picture trails it by a request rather than going blank). `None`
    /// until a first frame lands.
    pub fn frame(&self, ns: i64) -> Option<Arc<Frame>> {
        let bucket = bucket_of(ns);
        let st = lock(&self.shared);
        st.frames.iter().find(|f| f.bucket == bucket).or_else(|| st.frames.back()).cloned()
    }

    /// The file cannot give frames; the preview is its time and chapter alone.
    pub fn failed(&self) -> bool {
        lock(&self.shared).failed
    }
}

impl Drop for Thumbnailer {
    fn drop(&mut self) {
        lock(&self.shared).closed = true;
        self.shared.wake.notify_one();
    }
}

pub fn bucket_of(ns: i64) -> i64 {
    ns.max(0) / BUCKET_NS
}

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, State> {
    shared.state.lock().unwrap_or_else(|e| e.into_inner())
}

/// Keep `frame`, dropping the oldest past [`KEEP`].
fn keep(st: &mut State, frame: Frame) {
    st.frames.retain(|f| f.bucket != frame.bucket);
    st.frames.push_back(Arc::new(frame));
    while st.frames.len() > KEEP {
        st.frames.pop_front();
    }
}

fn log(msg: &str) {
    plx_base::eventlog::log(&format!("thumbs: {msg}"));
}

fn run(path: &Path, shared: &Shared) {
    let mut source = match Source::open(path) {
        Ok(s) => Some(s),
        Err(e) => {
            log(&format!("{}: {e}", path.display()));
            None
        }
    };
    loop {
        let ns = {
            let mut st = lock(shared);
            if source.is_none() {
                st.failed = true;
            }
            loop {
                if st.closed || st.failed {
                    return;
                }
                if let Some(ns) = st.wanted.take().or_else(|| st.backlog.pop_front()) {
                    break ns;
                }
                st = shared.wake.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some(src) = source.as_mut() else { return };
        match src.frame_at(ns) {
            Ok(rgba) => {
                keep(&mut lock(shared), Frame { bucket: bucket_of(ns), rgba });
                plx_machine::present::wake_from_worker();
            }
            Err(e) => {
                log(&format!("{} at {} ms: {e}", path.display(), ns / 1_000_000));
                // one bad seek is not the file: drop this request and serve the next
            }
        }
    }
}

/// The paused pipeline a worker seeks.
struct Source {
    pipeline: gst::Element,
    sink: gst_app::AppSink,
}

impl Source {
    fn open(path: &Path) -> Result<Self, String> {
        if !crate::player::gst_sink::gst_ready() {
            return Err("GStreamer is unavailable".into());
        }
        let uri = gst::glib::filename_to_uri(path, None).map_err(|e| e.to_string())?;
        let pipeline = gst::ElementFactory::make("playbin").property("uri", uri.as_str()).build().map_err(|e| e.to_string())?;
        // video only: no audio decoder, no subtitle overlay
        pipeline.set_property_from_str("flags", "video");
        let caps = format!("video/x-raw,format=RGBA,width={W},height={H},pixel-aspect-ratio=1/1");
        let bin = gst::parse::bin_from_description(
            &format!("videoconvert ! videoscale add-borders=true ! {caps} ! appsink name=thumb sync=false max-buffers=1 drop=true"),
            true,
        )
        .map_err(|e| e.to_string())?;
        let sink = bin
            .by_name("thumb")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .ok_or("no appsink")?;
        pipeline.set_property("video-sink", &bin);
        pipeline.set_state(gst::State::Paused).map_err(|e| e.to_string())?;
        let (res, _, _) = pipeline.state(gst::ClockTime::from_seconds(WAIT.as_secs()));
        res.map_err(|e| format!("could not preroll ({e})"))?;
        Ok(Self { pipeline, sink })
    }

    /// Seek to the keyframe nearest `ns` and take the frame the pipeline prerolls on.
    fn frame_at(&mut self, ns: i64) -> Result<Vec<u8>, String> {
        let flags = gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT | gst::SeekFlags::SNAP_NEAREST;
        self.pipeline
            .seek_simple(flags, gst::ClockTime::from_nseconds(ns.max(0) as u64))
            .map_err(|e| e.to_string())?;
        let (res, _, _) = self.pipeline.state(gst::ClockTime::from_seconds(WAIT.as_secs()));
        res.map_err(|e| format!("seek did not settle ({e})"))?;
        let sample = self
            .sink
            .try_pull_preroll(gst::ClockTime::from_seconds(WAIT.as_secs()))
            .ok_or("no frame")?;
        let buffer = sample.buffer().ok_or("an empty sample")?;
        let map = buffer.map_readable().map_err(|e| e.to_string())?;
        let want = (W * H * 4) as usize;
        if map.len() < want {
            return Err(format!("a {}-byte frame, wanted {want}", map.len()));
        }
        Ok(map[..want].to_vec())
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_keeps_one_frame_per_bucket_and_lets_the_oldest_go() {
        let mut st = State::default();
        for b in 0..(KEEP as i64 + 5) {
            keep(&mut st, Frame { bucket: b, rgba: Vec::new() });
        }
        assert_eq!(st.frames.len(), KEEP);
        assert_eq!(st.frames.front().unwrap().bucket, 5, "the oldest went first");
        keep(&mut st, Frame { bucket: 10, rgba: vec![1] });
        assert_eq!(st.frames.iter().filter(|f| f.bucket == 10).count(), 1, "a bucket is decoded once");
        assert_eq!(st.frames.back().unwrap().bucket, 10, "and the redecode is the newest");
    }

    #[test]
    fn a_time_falls_in_its_bucket() {
        assert_eq!(bucket_of(0), 0);
        assert_eq!(bucket_of(BUCKET_NS - 1), 0);
        assert_eq!(bucket_of(BUCKET_NS), 1);
        assert_eq!(bucket_of(-5), 0);
    }
}
