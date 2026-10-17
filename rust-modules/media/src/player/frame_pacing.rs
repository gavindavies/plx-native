//! **Which decoded picture the next swap shows** — chosen by its timestamp, not by when it arrived.
//!
//! The GStreamer sink's `appsink` used to hand each picture over at its due time (and then copy
//! it), and the render loop took whatever was newest when it composited. A 23.976 fps film on a
//! 60 Hz panel should hold every picture for exactly 2 or 3 refreshes, and steady jitter cannot
//! change that; but a picture that reached the loop more than about half a refresh later than the
//! one before it — a slow copy, a busy core — slipped a vsync, leaving a 4-refresh hold (65-68 ms)
//! and a short one beside it. Measured on a dual-core laptop: 17 and 25 of ~2,760 pictures over
//! two minutes.
//!
//! Now the sink releases pictures a little early ([`EARLY_NS`]) into a [`FrameQueue`], and the
//! render loop asks for the newest picture due by the vblank its swap will be shown at
//! ([`Vsync::next_vblank`], a phase-locked estimate from the swap completions). Both sides of that
//! comparison are smooth clocks, so the 2:3 cadence holds.

use std::collections::VecDeque;

/// How far ahead of its due time the sink releases a picture: comfortably more than a refresh
/// period, so the picture is queued before the composite that needs it.
pub(crate) const EARLY_NS: u64 = 50_000_000;

/// Pictures queued beyond the one on screen. Only reached while nothing composites (a hidden
/// window); the oldest goes first.
const QUEUE_CAP: usize = 8;

/// One decoded picture and the pipeline running time it is due at (`None`: show it at once).
pub(crate) struct Pending<T> {
    pub seq: u64,
    pub due: Option<u64>,
    pub item: T,
}

/// The picture on screen and those released ahead of their time.
pub(crate) struct FrameQueue<T> {
    shown: Option<Pending<T>>,
    pending: VecDeque<Pending<T>>,
}

impl<T> FrameQueue<T> {
    pub const fn new() -> Self {
        Self { shown: None, pending: VecDeque::new() }
    }

    pub fn push(&mut self, frame: Pending<T>) {
        self.pending.push_back(frame);
        if self.pending.len() > QUEUE_CAP {
            self.pending.pop_front();
        }
    }

    /// Advance to the newest picture due by `target` (pipeline running time) and return the one to
    /// show. With no target — paused, or no clock yet — the picture on screen stays, and the first
    /// one queued stands in when there is none (the prerolled picture of a paused Load).
    pub fn select(&mut self, target: Option<u64>) -> Option<&Pending<T>> {
        match target {
            Some(t) => {
                while self.pending.front().is_some_and(|f| f.due.is_none_or(|d| d <= t)) {
                    self.shown = self.pending.pop_front();
                }
            }
            None if self.shown.is_none() => self.shown = self.pending.pop_front(),
            None => {}
        }
        self.shown.as_ref()
    }

    /// Pictures are waiting behind the one on screen.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Forget the queued pictures and keep the one on screen (a flush restarts the timeline).
    pub fn drop_pending(&mut self) {
        self.pending.clear();
    }

    pub fn clear(&mut self) {
        self.shown = None;
        self.pending.clear();
    }
}

/// The display's vblank grid, estimated from swap completions (monotonic nanoseconds): a period
/// averaged over ordinary intervals and a phase pulled gently toward each completion, so a late
/// or early swap moves the grid by a fraction of its error rather than all of it.
pub(crate) struct Vsync {
    period: Option<f64>,
    anchor: Option<f64>,
    last: Option<u64>,
    /// An interval the period estimate rejected, and how many in a row have matched it. A steady
    /// run of them is a new refresh rate; the scattered gaps of frames that skipped presenting
    /// never agree long enough to count.
    candidate: Option<f64>,
    agreeing: u32,
}

/// Plausible refresh periods: 24 Hz to 125 Hz.
const MIN_PERIOD_NS: f64 = 8_000_000.0;
const MAX_PERIOD_NS: f64 = 42_000_000.0;
/// Matching rejected intervals in a row before the period is re-learned.
const RELEARN_AFTER: u32 = 30;

impl Vsync {
    pub const fn new() -> Self {
        Self { period: None, anchor: None, last: None, candidate: None, agreeing: 0 }
    }

    /// A swap completed at `t`.
    pub fn note_swap(&mut self, t: u64) {
        if let Some(prev) = self.last {
            let d = t.saturating_sub(prev) as f64;
            let plausible = (MIN_PERIOD_NS..=MAX_PERIOD_NS).contains(&d);
            match self.period {
                Some(p) if (d - p).abs() <= p / 8.0 => {
                    self.period = Some(p + (d - p) / 16.0);
                    self.candidate = None;
                }
                Some(_) => {
                    match self.candidate {
                        Some(c) if (d - c).abs() <= c / 8.0 => self.agreeing += 1,
                        _ => {
                            self.candidate = plausible.then_some(d);
                            self.agreeing = 0;
                        }
                    }
                    if self.agreeing >= RELEARN_AFTER {
                        self.period = Some(d);
                        self.anchor = None;
                        self.candidate = None;
                    }
                }
                None if plausible => self.period = Some(d),
                _ => {}
            }
        }
        self.last = Some(t);
        let t = t as f64;
        self.anchor = Some(match (self.anchor, self.period) {
            (Some(a), Some(p)) => {
                let grid = a + ((t - a) / p).round() * p;
                grid + (t - grid) / 8.0
            }
            _ => t,
        });
    }

    /// The first vblank at or after `now`, once a period has been learned.
    pub fn next_vblank(&self, now: u64) -> Option<u64> {
        let (a, p) = (self.anchor?, self.period?);
        let n = ((now as f64 - a) / p).ceil();
        Some((a + n * p).max(0.0) as u64)
    }

    #[cfg(test)]
    pub fn period(&self) -> Option<u64> {
        self.period.map(|p| p as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;
    /// 60 Hz and 23.976 fps, in nanoseconds.
    const VSYNC: f64 = 1e9 / 60.0;
    const FRAME: f64 = 1e9 * 1001.0 / 24000.0;

    fn frame(seq: u64, due: u64) -> Pending<u64> {
        Pending { seq, due: Some(due), item: seq }
    }

    #[test]
    fn the_newest_due_picture_is_shown_and_older_ones_are_passed() {
        let mut q = FrameQueue::new();
        for s in 1..=3 {
            q.push(frame(s, s * 10 * MS));
        }
        assert_eq!(q.select(Some(5 * MS)).map(|f| f.seq), None, "nothing is due yet");
        assert_eq!(q.select(Some(25 * MS)).map(|f| f.seq), Some(2));
        assert!(q.has_pending());
        assert_eq!(q.select(Some(26 * MS)).map(|f| f.seq), Some(2), "the picture holds until the next is due");
        assert_eq!(q.select(Some(30 * MS)).map(|f| f.seq), Some(3));
    }

    #[test]
    fn a_paused_queue_shows_its_first_picture_and_holds_it() {
        let mut q = FrameQueue::new();
        q.push(frame(1, 0));
        q.push(frame(2, 40 * MS));
        assert_eq!(q.select(None).map(|f| f.seq), Some(1));
        assert_eq!(q.select(None).map(|f| f.seq), Some(1));
        q.drop_pending();
        assert_eq!(q.select(Some(u64::MAX)).map(|f| f.seq), Some(1), "a flush keeps the picture on screen");
        q.clear();
        assert!(q.select(None).is_none());
    }

    #[test]
    fn a_picture_without_a_timestamp_is_due_at_once() {
        let mut q = FrameQueue::new();
        q.push(Pending { seq: 1, due: None, item: 1 });
        assert_eq!(q.select(Some(0)).map(|f| f.seq), Some(1));
    }

    #[test]
    fn an_uncomposited_queue_keeps_only_the_newest_pictures() {
        let mut q = FrameQueue::new();
        for s in 1..=20 {
            q.push(frame(s, s * MS));
        }
        assert_eq!(q.select(Some(0)).map(|f| f.seq), None);
        assert_eq!(q.select(Some(u64::MAX)).map(|f| f.seq), Some(20));
        assert_eq!(q.pending.len(), 0);
    }

    #[test]
    fn the_vblank_grid_locks_to_jittery_swaps_and_survives_a_missed_one() {
        let mut v = Vsync::new();
        assert!(v.next_vblank(0).is_none());
        let jitter = [0.0, 1.3, -0.8, 2.1, -1.9, 0.4];
        let mut t = 100.0 * MS as f64;
        for i in 0..600 {
            // One missed vblank at i == 300: the swap lands two periods after the previous one.
            t += if i == 300 { 2.0 * VSYNC } else { VSYNC };
            v.note_swap((t + jitter[i % jitter.len()] * MS as f64 / 10.0) as u64);
        }
        let p = v.period().unwrap() as f64;
        assert!((p - VSYNC).abs() < 50_000.0, "period {p}");
        let next = v.next_vblank(t as u64 + 3 * MS).unwrap() as f64;
        assert!((next - (t + VSYNC)).abs() < 0.5 * MS as f64, "next vblank {next} vs {}", t + VSYNC);
    }

    #[test]
    fn frames_that_skip_presenting_do_not_teach_a_period() {
        let mut v = Vsync::new();
        let mut t = 0u64;
        for _ in 0..100 {
            t += 16_666_667;
            v.note_swap(t);
        }
        // An idle screen presents now and then: gaps of 2, 3, 2, 5... refreshes.
        for i in 0..200u64 {
            t += 16_666_667 * (2 + i % 4);
            v.note_swap(t);
        }
        assert!((v.period().unwrap() as i64 - 16_666_667).abs() < 100_000);
    }

    #[test]
    fn a_new_refresh_rate_is_relearned() {
        let mut v = Vsync::new();
        let mut t = 0u64;
        for _ in 0..100 {
            t += 16_666_667;
            v.note_swap(t);
        }
        for _ in 0..100 {
            t += 20_000_000;
            v.note_swap(t);
        }
        assert!((v.period().unwrap() as i64 - 20_000_000).abs() < 100_000);
    }

    /// The regression. At 2.5 refreshes a picture, steady arrival jitter alone cannot break a 2:3
    /// cadence; what does is a picture reaching the render loop more than about half a refresh
    /// later than the one before it (a slow copy, a busy core). Pictures here arrive within ±1 ms,
    /// and about one in fifty is 9-25 ms late. Taking the newest arrival at the composite (the old
    /// rule) turns those into 1- and 4-refresh holds; choosing by timestamp against the vblank
    /// gives exactly 2 or 3 every time.
    #[test]
    fn a_23_976_fps_film_on_60_hz_holds_every_picture_two_or_three_refreshes() {
        // Deterministic pseudo-random arrival delays, in nanoseconds.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut jitter = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let steady = (seed % 2_000_001) as f64 - 1_000_000.0;
            let late = if (seed >> 32) % 50 == 0 { 9_000_000.0 + ((seed >> 40) % 16_000_001) as f64 } else { 0.0 };
            steady + late
        };
        let frames = 2_800usize;
        let mut arrive = Vec::with_capacity(frames);
        for n in 0..frames {
            arrive.push(n as f64 * FRAME + jitter());
        }
        let due: Vec<f64> = (0..frames).map(|n| n as f64 * FRAME).collect();
        // The composite runs 4 ms after each vblank (UI draw), the swap shows at the next vblank.
        let vblanks = (frames as f64 * FRAME / VSYNC) as usize;
        let holds = |shown: &[Option<usize>]| {
            let mut runs = vec![0usize; frames];
            for s in shown.iter().flatten() {
                runs[*s] += 1;
            }
            runs[1..frames - 1].to_vec()
        };

        // The old rule: the newest picture that has ARRIVED by the composite.
        let mut old = Vec::new();
        for k in 0..vblanks {
            let composite = k as f64 * VSYNC + 4.0 * MS as f64;
            old.push(arrive.iter().rposition(|&a| a <= composite));
        }
        let old_holds = holds(&old);
        let hist = |hs: &[usize]| (0..6).map(|n| hs.iter().filter(|&&h| h == n).count()).collect::<Vec<_>>();
        assert!(old_holds.iter().any(|&h| h == 4 || h == 1), "arrival jitter should break the cadence: {:?}", hist(&old_holds));

        // The new rule: the newest picture DUE by the vblank this composite is shown at.
        let mut v = Vsync::new();
        let mut q = FrameQueue::new();
        let mut next = 0usize;
        let mut new = Vec::new();
        for k in 0..vblanks {
            let composite = k as f64 * VSYNC + 4.0 * MS as f64;
            while next < frames && arrive[next] - EARLY_NS as f64 <= composite {
                q.push(Pending { seq: next as u64, due: Some(due[next] as u64), item: next });
                next += 1;
            }
            let target = v.next_vblank(composite as u64).unwrap_or(composite as u64);
            new.push(q.select(Some(target)).map(|f| f.item));
            v.note_swap(((k + 1) as f64 * VSYNC + 0.2 * MS as f64) as u64);
        }
        // The first vblanks lock the grid; judge the steady state.
        let new_holds = holds(&new);
        let settled = &new_holds[10..];
        assert!(settled.iter().all(|&h| h == 2 || h == 3), "holds {:?}", settled.iter().filter(|&&h| h != 2 && h != 3).count());
    }
}
