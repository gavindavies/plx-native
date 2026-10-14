//! **The player's read-ahead cache** (Settings › Cache; Kodi's "buffer mode", "memory size" and
//! "read factor"): a worker reads the file ahead of the demuxer into a bounded buffer, so a
//! high-bitrate remux on a network share rides out the link's dips instead of stalling the picture.
//!
//! It sits under [`crate::localfile::LocalFile::read_at`], the one door FFmpeg's AVIO reads through,
//! so nothing above it knows it is there. The buffer is one contiguous run of the file,
//! `[base, base + len)`:
//!
//! - **A read inside the run** is a copy. The run is trimmed behind the reader to [`BACK_BYTES`],
//!   so libavformat's short backward hops (a Matroska cluster re-read) still hit.
//! - **A read at the run's end** waits for the worker, which is reading exactly there.
//! - **A read anywhere else** is a seek: it is served straight from the file, and the run restarts
//!   at the byte after it (a new `generation`, so a chunk the worker was reading for the old
//!   position is dropped rather than spliced in).
//!
//! The worker fills up to the configured size ahead of the reader in [`CHUNK`]-byte reads. With a
//! read factor it paces itself at that multiple of the file's average bitrate — but only while the
//! buffer holds [`LOW_WATER`] or more: a reader that is waiting is never made to wait for pacing.
//!
//! [`buffered_until`] publishes how far the run reaches, for the scrubber's buffered band. One
//! playback reads at a time, so it is one process-wide value, cleared when the cache closes.

use std::collections::VecDeque;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicI64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// One worker read.
const CHUNK: usize = 1024 * 1024;
/// How much is kept behind the reader for libavformat's backward hops.
const BACK_BYTES: usize = 4 * 1024 * 1024;
/// Below this much ahead the worker reads flat out, read factor or not.
const LOW_WATER: usize = 8 * 1024 * 1024;

/// The end of the run the cache holds (a file offset), -1 while no cache is open.
static BUFFERED_UNTIL: AtomicI64 = AtomicI64::new(-1);
/// The size of the file the open cache reads.
static BUFFERED_SIZE: AtomicI64 = AtomicI64::new(0);

/// How far into the playing file the cache has read, as `(offset, file size)`. `None` while no
/// read-ahead is open (it is off, or the file is not one it covers).
pub fn buffered_until() -> Option<(i64, i64)> {
    let until = BUFFERED_UNTIL.load(Relaxed);
    (until >= 0).then(|| (until, BUFFERED_SIZE.load(Relaxed)))
}

struct State {
    /// The file offset of `buf[0]`.
    base: i64,
    buf: VecDeque<u8>,
    /// Bumped by every restart: a chunk read for an older generation is dropped.
    generation: u64,
    /// The reader's next offset: what "ahead" is measured from.
    reader: i64,
    /// The reader is blocked at the run's end.
    waiting: bool,
    eof: bool,
    /// A worker read failed: the errno, handed to the reader at the run's end.
    error: Option<i32>,
    closed: bool,
}

impl State {
    fn end(&self) -> i64 {
        self.base + self.buf.len() as i64
    }
    fn ahead(&self) -> usize {
        (self.end() - self.reader).max(0) as usize
    }
    /// Start the run over at `at`.
    fn restart(&mut self, at: i64) {
        self.base = at;
        self.buf.clear();
        self.generation += 1;
        self.reader = at;
        self.eof = false;
        self.error = None;
        BUFFERED_UNTIL.store(at, Relaxed);
    }
}

struct Shared {
    state: Mutex<State>,
    /// Woken when the run grows, the reader moves, or the cache closes.
    changed: Condvar,
    limit: usize,
    /// The fill pace in bytes per second; 0 = unlimited or not known yet.
    pace: AtomicI64,
    /// The read factor, kept to turn a duration into a pace.
    factor: Option<u32>,
    size: i64,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A file read through the cache. Dropping it stops the worker (which is detached: a read stuck on
/// a dead share must not hold up the player's teardown).
pub struct ReadAhead {
    file: Arc<File>,
    shared: Arc<Shared>,
}

impl ReadAhead {
    /// Start reading `file` (`size` bytes) ahead, holding up to `limit` bytes, paced at `factor`
    /// times the bitrate once [`Self::set_duration`] says what it is.
    pub fn start(file: Arc<File>, size: i64, limit: usize, factor: Option<u32>) -> Option<ReadAhead> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                base: 0,
                buf: VecDeque::new(),
                generation: 0,
                reader: 0,
                waiting: false,
                eof: false,
                error: None,
                closed: false,
            }),
            changed: Condvar::new(),
            limit: limit.max(2 * CHUNK),
            pace: AtomicI64::new(0),
            factor,
            size,
        });
        BUFFERED_SIZE.store(size, Relaxed);
        BUFFERED_UNTIL.store(0, Relaxed);
        let (worker, from) = (shared.clone(), file.clone());
        if !plx_base::task::spawn_small("read-ahead", move || fill(&from, &worker)) {
            BUFFERED_UNTIL.store(-1, Relaxed);
            return None;
        }
        Some(ReadAhead { file, shared })
    }

    /// The file's running time, which turns the read factor into a pace: `factor × size ÷ duration`.
    pub fn set_duration(&self, duration_ns: i64) {
        if let (Some(f), true) = (self.shared.factor, duration_ns > 0) {
            let bytes_per_s = (self.shared.size as i128 * 1_000_000_000 / duration_ns as i128) as i64;
            self.shared.pace.store(bytes_per_s.saturating_mul(i64::from(f)), Relaxed);
        }
    }

    /// [`crate::localfile::LocalFile::read_at`]'s answer, through the cache.
    pub fn read_at(&self, dst: &mut [u8], offset: i64) -> i32 {
        if dst.is_empty() || offset < 0 {
            return 0;
        }
        let mut st = self.shared.lock();
        loop {
            if offset >= st.base && offset < st.end() {
                let at = (offset - st.base) as usize;
                let n = dst.len().min(st.buf.len() - at);
                let (a, b) = st.buf.as_slices();
                if at < a.len() {
                    let first = n.min(a.len() - at);
                    dst[..first].copy_from_slice(&a[at..at + first]);
                    dst[first..n].copy_from_slice(&b[..n - first]);
                } else {
                    dst[..n].copy_from_slice(&b[at - a.len()..at - a.len() + n]);
                }
                st.reader = offset + n as i64;
                // keep only BACK_BYTES behind the reader
                let behind = (st.reader - st.base) as usize;
                if behind > BACK_BYTES {
                    let drop = behind - BACK_BYTES;
                    st.buf.drain(..drop);
                    st.base += drop as i64;
                }
                drop(st);
                self.shared.changed.notify_all();
                return n as i32;
            }
            if offset == st.end() {
                if st.eof || offset >= self.shared.size {
                    return 0;
                }
                if let Some(errno) = st.error.take() {
                    return -errno;
                }
                // the worker is reading exactly here: wait for it
                st.reader = offset;
                st.waiting = true;
                self.shared.changed.notify_all();
                st = self.shared.changed.wait(st).unwrap_or_else(|e| e.into_inner());
                st.waiting = false;
                continue;
            }
            // A seek: answer this read from the file and start the run after it.
            drop(st);
            let n = direct(&self.file, dst, offset);
            let mut st = self.shared.lock();
            st.restart(offset + i64::from(n.max(0)));
            drop(st);
            self.shared.changed.notify_all();
            return n;
        }
    }
}

impl Drop for ReadAhead {
    fn drop(&mut self) {
        self.shared.lock().closed = true;
        self.shared.changed.notify_all();
        BUFFERED_UNTIL.store(-1, Relaxed);
    }
}

/// One positional read with `EINTR` retried: `>0` bytes, `0` at the end, `<0` an errno.
pub(crate) fn direct(file: &File, dst: &mut [u8], offset: i64) -> i32 {
    let len = dst.len().min(i32::MAX as usize);
    loop {
        match file.read_at(&mut dst[..len], offset as u64) {
            Ok(n) => return n as i32,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return -e.raw_os_error().unwrap_or(libc::EIO),
        }
    }
}

/// The worker: keep the run `limit` bytes ahead of the reader.
fn fill(file: &File, shared: &Shared) {
    let mut chunk = vec![0u8; CHUNK];
    // The pace's clock: bytes read since `since`, restarted with each generation.
    let (mut since, mut read, mut paced_gen) = (Instant::now(), 0u64, u64::MAX);
    loop {
        let (at, generation, waiting, ahead) = {
            let mut st = shared.lock();
            loop {
                if st.closed {
                    return;
                }
                let room = st.ahead() + CHUNK <= shared.limit;
                if room && !st.eof && st.error.is_none() {
                    break;
                }
                st = shared.changed.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            (st.end(), st.generation, st.waiting, st.ahead())
        };
        if generation != paced_gen {
            (since, read, paced_gen) = (Instant::now(), 0, generation);
        }
        // Pacing: hold the fill to the pace once there is a cushion, never while the reader waits.
        let pace = shared.pace.load(Relaxed);
        if pace > 0 && !waiting && ahead >= LOW_WATER {
            let due = Duration::from_secs_f64(read as f64 / pace as f64);
            let spent = since.elapsed();
            if due > spent {
                let st = shared.lock();
                // a seek, a waiting reader or a close wakes this early
                let _ = shared.changed.wait_timeout(st, (due - spent).min(Duration::from_millis(250)));
                continue;
            }
        }
        let want = CHUNK.min((shared.size - at).max(0) as usize);
        let n = if want == 0 { 0 } else { direct(file, &mut chunk[..want], at) };
        let mut st = shared.lock();
        if st.generation != generation || st.end() != at {
            continue; // the reader moved while we read: this chunk is for a run that is gone
        }
        match n {
            0 => st.eof = true,
            n if n < 0 => st.error = Some(-n),
            n => {
                st.buf.extend(&chunk[..n as usize]);
                read += n as u64;
                BUFFERED_UNTIL.store(st.end(), Relaxed);
            }
        }
        drop(st);
        shared.changed.notify_all();
    }
}

/// **Is the file at `path` on a network filesystem** — a CIFS/SMB mount, NFS, or a FUSE mount
/// (which is how gvfs mounts an SMB share in userspace)? What [`ReadAhead`]'s default
/// "network shares" mode turns on for.
pub fn on_network(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    const CIFS: i64 = 0xFF53_4D42;
    const SMB2: i64 = 0xFE53_4D42;
    const SMB: i64 = 0x517B;
    const NFS: i64 = 0x6969;
    const FUSE: i64 = 0x6573_5546;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return false;
    }
    // `f_type` is a signed word of either width: compare its low 32 bits.
    let kind = (st.f_type as i64) & 0xFFFF_FFFF;
    matches!(kind, CIFS | SMB2 | SMB | NFS | FUSE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_of(len: usize, name: &str) -> (std::path::PathBuf, Vec<u8>) {
        let dir = plx_base::testscratch::process_dir(&format!("readahead-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.bin");
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        (path, data)
    }

    fn cache(path: &std::path::Path, limit: usize) -> ReadAhead {
        let f = Arc::new(File::open(path).unwrap());
        let size = f.metadata().unwrap().len() as i64;
        ReadAhead::start(f, size, limit, None).unwrap()
    }

    #[test]
    fn a_sequential_read_returns_the_file_byte_for_byte() {
        let _g = plx_base::testlock::serial();
        let (path, data) = file_of(5 * CHUNK + 777, "seq");
        let ra = cache(&path, 3 * CHUNK);
        let (mut out, mut buf, mut off) = (Vec::new(), vec![0u8; 32 * 1024 + 5], 0i64);
        loop {
            let n = ra.read_at(&mut buf, off);
            assert!(n >= 0);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
            off += n as i64;
        }
        assert_eq!(out.len(), data.len());
        assert!(out == data, "byte for byte");
    }

    #[test]
    fn a_seek_is_answered_at_once_and_the_run_restarts_after_it() {
        let _g = plx_base::testlock::serial();
        let (path, data) = file_of(6 * CHUNK, "seek");
        let ra = cache(&path, 4 * CHUNK);
        let mut buf = vec![0u8; 1000];
        assert_eq!(ra.read_at(&mut buf, 0), 1000);
        let far = 4 * CHUNK as i64 + 123;
        assert_eq!(ra.read_at(&mut buf, far), 1000);
        assert_eq!(&buf[..], &data[far as usize..far as usize + 1000]);
        // ...and the next read follows on from the run
        assert_eq!(ra.read_at(&mut buf, far + 1000), 1000);
        assert_eq!(&buf[..], &data[far as usize + 1000..far as usize + 2000]);
        // a short hop back stays in the run
        assert_eq!(ra.read_at(&mut buf, far + 500), 1000);
        assert_eq!(&buf[..], &data[far as usize + 500..far as usize + 1500]);
        // past the end
        assert_eq!(ra.read_at(&mut buf, data.len() as i64), 0);
    }

    #[test]
    fn the_run_fills_up_to_its_limit_and_no_further() {
        let _g = plx_base::testlock::serial();
        let (path, _) = file_of(10 * CHUNK, "limit");
        let ra = cache(&path, 3 * CHUNK);
        let mut buf = vec![0u8; 16];
        assert_eq!(ra.read_at(&mut buf, 0), 16);
        let deadline = Instant::now() + Duration::from_secs(5);
        while buffered_until().map_or(0, |(u, _)| u) < 2 * CHUNK as i64 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(50));
        let (until, size) = buffered_until().unwrap();
        assert_eq!(size, 10 * CHUNK as i64);
        assert!(until >= 2 * CHUNK as i64 && until <= 16 + 3 * CHUNK as i64, "filled to the limit: {until}");
        drop(ra);
        assert_eq!(buffered_until(), None, "a closed cache publishes nothing");
    }

    #[test]
    fn a_local_scratch_file_is_not_on_a_network_share() {
        let (path, _) = file_of(10, "local");
        // the test machine's scratch directory is a local disk (or tmpfs), never a share
        assert!(!on_network(&path) || std::env::var_os("PLX_SCRATCH_ON_NETWORK").is_some());
    }
}
