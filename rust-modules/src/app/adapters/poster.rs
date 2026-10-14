//! The application's SOURCE half of image caching (restructure spec §10): interning
//! `(source, path, w, h, png)` into an opaque [`PosterKey`] (a slot index), the decode workers that
//! read the library's artwork files, the prefetch gate and the slot LRU. It delivers a decoded image as `tex::PosterReady` and holds NO texture: GL
//! residency is the library's [`plx_ui::tex::TexCache`], reached through `ui::tex`'s free
//! functions, and the seam between the two halves is the key. The library never names this
//! module; it sees it as the [`tex::Source`] installed at [`init`].
//!
//! A slot's lifecycle: EMPTY → WANT (claimed by a draw's miss or a prefetch) → LOADING (a worker
//! fetches + decodes off the lock) → DECODED (pixels waiting on the main thread) → READY (the
//! pixels were handed to the cache by [`drain_decoded`]; the cache uploads them in PREPARE),
//! EVICTED (the render cache could not keep that key resident, through rejection or pressure), or
//! FAILED / RETRY (a transient failure parked under bounded backoff). EVICTED does NOT re-arm on
//! a prefetch: [`lookup`]'s `P_EVICTED` branch answers a `Touch::Warm` probe with dormancy before
//! the cooldown gate is even reached, so a background prefetch walking past an evicted key never
//! revives it. Only a `Touch::Draw` probe re-arms the slot — so a poster or backdrop that lost
//! residency off-screen does not quietly reappear before something asks to show it again; the pop
//! that eviction caused stays on screen until the draw that wants the key re-requests it. The
//! source cannot offer another upload from retained pixels because ownership moved to the render
//! cache and retaining a second CPU copy of the entire 44 MiB GL pool would defeat the memory
//! ceiling. A later demand reads and decodes the image file again, a cost bounded by the residency
//! thrash guard, the 250 ms–8 s cooldown backoff and the Draw-only gate above. A READY slot
//! recycled by [`victim`] frees its cache entry on the way out.
//!
//! Rust port of the old src/posters.c; rewritten on std::sync (a `Mutex<Store>` + `Condvar` +
//! two `task::spawn` workers). The decoded-pixel pointer is stored as an address (usize) so the
//! shared `Store` stays `Send`.
//!
//! ## Every entry point names a SOURCE
//!
//! A [`ServerId`] rides every call and sits in the slot as the other half of its identity. The
//! local library's artwork ([`plx_data::local::ART_SRV`]) is the one source that loads; any other
//! id is a key that settles as failed.
mod trace;

use plx_gfx::img;
use plx_machine::machine::PosterKey;
use plx_ui::tex::{self, Decoded, PosterError, PosterReady, Tex, Uploader, Warm};
use std::os::raw::{c_int, c_uchar, c_uint};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

/// The source a slot's art comes from — the `u16` every `tex::Source` call carries. Only the local
/// library's ([`plx_data::local::ART_SRV`]) resolves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ServerId(u16);

impl ServerId {
    const UNSET: ServerId = ServerId(u16::MAX);
    fn from_raw(raw: u16) -> Self {
        ServerId(raw)
    }
    fn raw(self) -> u16 {
        self.0
    }
    fn is_local(self) -> bool {
        self.0 == plx_data::local::ART_SRV
    }
}

// Per-frame GL-upload counters for the frame-drop detector (app/run.rs): each upload the cache
// performs through `GfxUploader` is a synchronous glTexImage2D on the main thread — the prime
// suspect for scroll judder. `take_upload_stats` reads-and-resets (call once per frame).
static UP_CT: AtomicU32 = AtomicU32::new(0);
static UP_PX: AtomicU64 = AtomicU64::new(0);
/// (uploads, total pixels) since the last call; resets both. Main-thread, once per frame.
pub(crate) fn take_upload_stats() -> (u32, u64) {
    (
        UP_CT.swap(0, Ordering::Relaxed),
        UP_PX.swap(0, Ordering::Relaxed),
    )
}

const PT_CAP: usize = 64;
/// Decoded pixels waiting for upload, independent of the persistent and GPU cache budgets.
/// Existing active decodes can exceed this threshold; no further demand work starts until
/// the upload queue drains. This prevents a fast disk from queuing 64 large backdrops.
const DECODE_BACKLOG_MAX: usize = 8 * 1024 * 1024;
static BACKLOG_PEAK: AtomicU64 = AtomicU64::new(0);

fn decoded_slot_bytes(slots: &[Pslot; PT_CAP]) -> usize {
    slots.iter().filter(|s| s.state == P_DECODED)
        .map(|s| s.pw.max(0) as usize * s.ph.max(0) as usize * 4).sum()
}

fn decode_admitted(slots: &[Pslot; PT_CAP], pending_bytes: usize) -> bool {
    decoded_slot_bytes(slots).saturating_add(pending_bytes) < DECODE_BACKLOG_MAX
}

fn note_backlog(slots: &[Pslot; PT_CAP]) -> usize {
    let bytes = decoded_slot_bytes(slots).saturating_add(tex::pending_bytes());
    BACKLOG_PEAK.fetch_max(bytes as u64, Ordering::Relaxed);
    bytes
}

/// Bytes a slot's key array holds, NUL included — see [`Pslot::key`] for the cliff at the end of
/// it, and [`KEY_MAX`] for the gate that keeps requests off it.
const PT_KEYLEN: usize = 256;
/// The longest built path that survives the round trip into a slot intact. Derived from
/// [`PT_KEYLEN`] rather than written as a literal, so the gate cannot drift from the array it
/// gates.
const KEY_MAX: usize = PT_KEYLEN - 1;

/// Can this key ever resolve to a picture? The store's ONE precondition, and it lives here for the
/// same reason [`victim`] and [`idle_of`] were split out: it was duplicated across the entry
/// points, none of those copies could be tested (reaching them drags in `gfx::delete_tex`, which
/// no host test binary links), and the array it protects belongs to [`lookup`], not to whoever
/// built the string.
///
/// **Empty** means no request could be built — an unknown server, or an item the server gave no
/// art for (see [`poster_key`]).
///
/// **Longer than [`KEY_MAX`]** means [`set_key`] would TRUNCATE it, after which the stored key can
/// never equal the probe that built it: every frame misses, claims a fresh slot and evicts a real
/// poster — the whole store thrashing over one tile. `poster_key` refuses to hand such a path out,
/// but it is not the only thing that can reach `lookup` (the three entry points take a raw
/// `*const c_char`), so the check that matters is the one at the array.
///
/// Either way the answer is the same: no slot is claimed, and the tile draws its skeleton.
fn is_fetchable(key: &str) -> bool {
    !key.is_empty() && key.len() <= KEY_MAX
}
const P_EMPTY: c_int = 0;
const P_WANT: c_int = 1;
const P_LOADING: c_int = 2;
const P_DECODED: c_int = 3;
/// The pixels were handed to the render cache; whether they are UPLOADED yet is the cache's.
const P_READY: c_int = 5;
const P_FAILED: c_int = 6;
/// The render cache could not keep this key resident, through rejection or pressure. No pixels
/// are retained here and no fetch starts until the key is demanded again; see the module
/// lifecycle note for why.
const P_EVICTED: c_int = 8;

/// Speculation may occupy one worker, never both. That leaves capacity for a visible miss that
/// arrives after the prefetch began; workers cannot interrupt a fetch already off-lock.
const PREFETCH_OUTSTANDING_MAX: usize = 1;

/// How a store lookup treats the slot it lands on. The difference IS the prefetch's safety story.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Touch {
    /// A draw: bump the LRU clock and stamp the frame (evict-protect — a slot this frame asked for
    /// must never be a victim, or a full screen of tiles would evict each other).
    Draw,
    /// A prefetch: neither. See [`poster_warm`].
    Warm,
    /// An AT-REST lookahead warm (the Library grid's next rows, `screens/src/library/lookahead.rs`). A prefetch
    /// like [`Touch::Warm`] in every respect but its slot: it is stamped like a draw (a fresh LRU
    /// age, so it is not the next miss's first victim) and flagged [`Pslot::ahead`], and it picks
    /// its own victim ([`ahead_victim`]) so two lookahead warms cannot recycle each other. See the
    /// "Lookahead" note at [`AHEAD_PROTECT`].
    Ahead,
}

// `Warm` — what a prefetch did — is the library's `ui::tex::Warm` now (re-exported through
// the `use` above): the prefetch loops in `ui/` read it without naming this module.

#[derive(Clone, Copy)]
struct Pslot {
    // Full /photo/:/transcode request path = store key. NB set_key TRUNCATES to KEY_MAX bytes +
    // NUL: a key at/over PT_KEYLEN would never match its probe again (the text.rs glyph cache had
    // this exact bug at 96 — fixed with klen there), so every frame would miss, claim a fresh slot
    // and evict a real poster — the whole store thrashing over one tile. `poster_key` now REFUSES
    // to hand out a path this cannot hold (see [`KEY_MAX`]), which turns that into one skeleton
    // tile; if the shape ever routinely needs more room, key this the klen way instead of raising
    // the number.
    key: [u8; PT_KEYLEN],
    /// Which server this art was asked of — the OTHER half of the slot's identity, and what the
    /// worker dials. Two servers can hand out the same `/photo/:/transcode?url=/library/metadata/
    /// 42/thumb/…` path (their rating keys are both server-local integers from 1), so a key alone
    /// names a slot only while there is one server.
    srv: ServerId,
    pw: c_int,
    ph: c_int,
    px: usize, // decoded RGBA ptr as address (0 = none) — keeps Pslot Send
    state: c_int,
    /// A draw has asked for this key. Workers serve visible work before speculative warms.
    visible: bool,
    /// Claimed by an at-rest lookahead warm ([`Touch::Ahead`]) and not yet drawn. Cleared by the
    /// first Draw hit: a lookahead card that scrolled into view is an ordinary slot from then on.
    ahead: bool,
    use_: c_uint,  // LRU clock
    gen: c_uint,   // bumped on eviction; stale-decode guard
    cache_gen: u64, // account epoch captured when queued, before any worker can race sign-out
    frame: c_uint, // last frame poster_get touched it (evict-protect)
    /// The app-clock tick of this slot's most recent READY→EVICTED transition (residency
    /// pressure, [`PosterSource::unresident`] — never a fresh claim). `None` until the first
    /// eviction. Compared against [`EVICT_THRASH_WINDOW_MS`] on the NEXT eviction to tell an
    /// isolated, ordinary LRU turnover from a continuation of an ongoing thrash episode; see
    /// [`evict_was_rapid`].
    evicted_at: Option<u32>,
    /// Consecutive RAPID re-evictions (see [`evict_was_rapid`]) — the residency thrash guard's
    /// exponent. Reset to 0 the instant an eviction is NOT rapid, so an ordinary, occasional
    /// LRU turnover never accumulates backoff it does not deserve.
    evict_attempts: u8,
    /// The app-clock tick before which [`lookup`] refuses to re-arm this EVICTED slot — `None`
    /// means no cooldown is owed. Set only once `evict_attempts` shows an ongoing thrash episode:
    /// the FIRST eviction of a key always re-arms on its very next Draw probe, which is the
    /// recovery [`lookup`]'s P_EVICTED branch exists for and the case
    /// `a_ready_source_hit_recovers_after_its_texture_is_evicted` pins. See [`evict_backoff`].
    evict_cooldown_until: Option<u32>,
    /// Latches that THIS `evict_cooldown_until`
    /// deadline has already requested its one present, so [`invalidate_due_evictions`] does not
    /// invalidate every frame for an off-screen slot nothing draws. Reset whenever a fresh
    /// cooldown is armed ([`PosterSource::unresident`]) or the slot's eviction history is
    /// otherwise cleared (recovery, recycle) — a new deadline is something to wake for again.
    evict_wake_sent: bool,
}
impl Pslot {
    const ZERO: Pslot = Pslot {
        key: [0; PT_KEYLEN],
        srv: ServerId::UNSET,
        pw: 0,
        ph: 0,
        px: 0,
        state: P_EMPTY,
        visible: false,
        ahead: false,
        use_: 0,
        gen: 0,
        cache_gen: 0,
        frame: 0,
        evicted_at: None,
        evict_attempts: 0,
        evict_cooldown_until: None,
        evict_wake_sent: false,
    };
}

/// **The residency thrash guard (Codex P1 review on PR #182, `lookup`'s P_EVICTED branch).**
/// A `Touch::Warm` probe never reaches this guard at all — it is turned away before the branch
/// even looks at the cooldown (see the branch itself) — so what is left for this guard to bound is
/// narrower than the original review reads. Originally (before the render cache learned to protect
/// on-screen art, `tex.rs`'s "on-screen art stays; arrivals lose") a working set that genuinely
/// could not fit under [`tex::TEX_RESIDENT_BYTES_MAX`] had the render cache evict key A to make
/// room, a DRAW of A re-arm it, A's eventual upload evict key B, a DRAW of B re-arm IT, and so on
/// with no bound — continuous fetch/decode/upload every frame instead of settling, driven entirely
/// by on-screen demand rather than off-screen prefetch. `tex::TexCache::evict_for` no longer lets
/// that particular cycle happen at all: it will not evict an entry drawn in the current or the
/// last completed frame, so a genuinely over-budget on-screen working set now REFUSES the newer
/// arrival outright (a steady skeleton, `PosterSource::unresident`'s `refused` case) rather than
/// evicting the older one to admit it. What is left for this guard to bound is the RECOVERY side
/// of that refusal: a refused arrival's slot still goes `P_EVICTED` and still needs re-arming, and
/// under sustained pressure a key that keeps getting re-armed only to be refused again the moment
/// its cooldown clears is still churning fetch/decode/upload without ever landing — this guard
/// paces how often that retry is allowed to turn, the same shape of problem the original review
/// named even though the mechanism producing it changed.
///
/// The guard is two pure decisions, both keyed off wall-clock ticks so they cost nothing under the
/// store lock and are host-testable without a real elapsed second (mirrors the residency log's
/// own throttle):
///
/// - [`evict_was_rapid`]: was the PREVIOUS eviction of this key recent enough that this new one is
///   a continuation of an ongoing churn episode, rather than an isolated, ordinary LRU turnover?
///   Ten seconds comfortably exceeds one fetch+decode+upload round trip (the window this guard has
///   to see through), so a slot that settles for that long always gets treated as fresh again —
///   the window is sized against the round trip, not against how a person actually browses, so a
///   user who leaves a screen and comes back inside ten seconds can still land inside an ongoing
///   episode and pay whatever cooldown was already running, with no sustained overload involved.
/// - [`evict_backoff`]: given consecutive rapid re-evictions, how long does the NEXT re-arm wait?
///   It paces retries against a cache that is merely full, and a screen whose working set has
///   genuinely settled should feel that within a few seconds.
///
/// **What the bound actually guarantees.** A key's FIRST eviction always has `evicted_at == None`
/// beforehand, so [`evict_was_rapid`] answers `false`, `evict_attempts` stays 0, and no cooldown is
/// set at all — the ordinary one-eviction-then-recovery case this PR exists to fix re-arms on the
/// very next Draw probe, exactly as before. Only a key that is evicted AGAIN inside the thrash
/// window — proof that something is still churning it — pays a cooldown, and that cooldown is
/// bounded ([`evict_backoff`] caps at 8 s): a Draw probe of the key is guaranteed another turn to
/// ATTEMPT recovery within that bound. That is narrower than it may sound — it is eligibility for
/// another attempt, not a promise the attempt loads successfully, and not durable residency: under
/// sustained pressure the texture can be evicted again the instant it lands.
const EVICT_THRASH_WINDOW_MS: u32 = 10_000;

/// Pure half of the guard: given the tick of a key's previous eviction (`None` before its first),
/// does a new eviction at `now` land inside the thrash window?
fn evict_was_rapid(prev: Option<u32>, now: u32) -> bool {
    prev.is_some_and(|t| now.wrapping_sub(t) < EVICT_THRASH_WINDOW_MS)
}


/// How long the NEXT re-arm of a key waits after its `n`th consecutive RAPID re-eviction (see
/// [`evict_was_rapid`]): 250 ms, 500 ms, 1 s, 2 s, 4 s, then 8 s for good. `n == 0` never reaches
/// this — see the guard's module doc for why a key's first eviction sets no cooldown at all.
fn evict_backoff(evict_attempts: u8) -> Duration {
    let millis = 250u64 << evict_attempts.saturating_sub(1).min(5);
    Duration::from_millis(millis.min(8_000))
}
/// Has a P_EVICTED slot's cooldown cleared? Wrap-safe; `None` (no cooldown was
/// ever set) is always due.
///
/// **In a frame dump (`plx_gfx::dump`) every cooldown is over.** The dump holds the app clock still
/// while it waits for debt to clear, so a cooldown armed from that clock never expires and the
/// re-arm it paces would never come (a hold with no way out); and a cooldown that did expire would
/// make the frame it expires on a function of how long the host took. The dump's own residency is
/// bounded and logical, so the thrash this guard paces does not arise there.
fn evict_cooldown_due(cooldown_until: Option<u32>, now: u32) -> bool {
    evict_cooldown_due_in(plx_gfx::dump::armed(), cooldown_until, now)
}

/// [`evict_cooldown_due`] with the dump switch passed in, for a host test.
#[inline]
fn evict_cooldown_due_in(dump: bool, cooldown_until: Option<u32>, now: u32) -> bool {
    if dump {
        return true;
    }
    match cooldown_until {
        None => true,
        Some(t) => now.wrapping_sub(t) < u32::MAX / 2,
    }
}

/// Is an EVICTED slot's cooldown deadline due for its one wake — as opposed to [`evict_cooldown_due`],
/// which also answers `true` for a slot that never had a cooldown at all? A slot with no deadline has
/// nothing to expire and needs no proactive wake — its very next Draw probe already re-arms it, same
/// as today. Only a slot an [`evict_backoff`] deadline is ticking against needs [`invalidate_due_evictions`]
/// to ask for the frame that lets it recover on a quiet screen. The tick comparison wraps, like
/// every `app::clock` comparison.
fn evict_wake_due(s: &Pslot, now: u32) -> bool {
    s.state == P_EVICTED && s.evict_cooldown_until.is_some_and(|t| now.wrapping_sub(t) < u32::MAX / 2)
}

/// The eviction cooldown's wake, and the fix for the gap the
/// adjudication on PR #182 flagged: nothing previously asked the screen to draw again when a
/// cooldown deadline passed, so on a quiet screen a slot recovered only when something ELSE
/// happened to trigger a frame — later than its own deadline, sometimes much later. Same shape,
/// same reason for the `evict_wake_sent` latch: without it, an expired cooldown on a slot nothing
/// draws would invalidate every loop iteration forever, undoing the idle behaviour the app depends
/// on. The wake only ever asks for one present; it does not itself re-arm anything — re-arming
/// stays Draw-only, through `lookup`'s P_EVICTED branch, exactly as before.
fn invalidate_due_evictions(slots: &mut [Pslot; PT_CAP], now: u32) {
    let mut due = false;
    for s in slots.iter_mut().filter(|s| !s.evict_wake_sent && evict_wake_due(s, now)) {
        s.evict_wake_sent = true;
        due = true;
    }
    if due {
        plx_machine::idle::invalidate();
    }
}


/// The store's whole identity rule: does `s` hold the picture `key` asks for, from the same
/// source? The `srv` compare goes first because this runs for every slot of every probe of every
/// visible tile.
fn same_art(s: &Pslot, srv: ServerId, key: &[u8]) -> bool {
    s.state != P_EMPTY && s.srv == srv && key_bytes(s) == key
}

struct Store {
    slots: [Pslot; PT_CAP],
    clock: c_uint,
    frame: c_uint,
    quit: bool,
    workers: Vec<JoinHandle<()>>,
}
impl Store {
    const fn new() -> Store {
        Store {
            slots: [Pslot::ZERO; PT_CAP],
            clock: 0,
            frame: 0,
            quit: false,
            workers: Vec::new(),
        }
    }
}

static STORE: Mutex<Store> = Mutex::new(Store::new());

/// **Frame dump only: the first art fetch that failed.** A failed fetch (a 404, bytes that do not
/// decode, a refused connect) leaves its card on a placeholder forever, and a dump holds on a
/// placeholder, so without this the run would sit to its wall-clock timeout and name a hold
/// reason instead of the request. The driver reads it after every draw ([`dump_failure`]) and ends
/// the run naming the key. A transient failure is recorded too: its retry waits on the app clock,
/// which a dump holds still, so it could never come. Never written outside a dump.
static DUMP_FAILURE: Mutex<Option<String>> = Mutex::new(None);

fn note_dump_failure(key: &[u8], transient: bool) {
    let mut g = DUMP_FAILURE.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        let key = String::from_utf8_lossy(key).into_owned();
        *g = Some(format!(
            "an art request failed ({}): {key}",
            if transient { "transient" } else { "final: a missing file or bytes that do not decode" }
        ));
    }
}

static CV: Condvar = Condvar::new();

fn store() -> MutexGuard<'static, Store> {
    STORE.lock().unwrap_or_else(|e| e.into_inner())
}

fn key_bytes(s: &Pslot) -> &[u8] {
    plx_base::cbuf::as_bytes(&s.key)
}
fn set_key(s: &mut Pslot, key: &str) {
    plx_base::cbuf::set_bytes(&mut s.key, key);
}

// (server, path, w, h, png) → built transcode path, memoised. resolve_tex_wh_on re-derives the key for
// every visible art tile every frame (~25-40 tiles × 60fps); the QueryBuilder + token clone behind
// image_transcode_path was ~15k heap allocs/sec of steady-state waste for keys that never change.
// MAIN-thread only (all poster_key callers are draw paths).
//
// The SERVER is part of the key and the token generation is part of the VALUE. Both matter, and
// neither is bookkeeping: the built path carries a token, so keying without the server would let
// server B's card be built from A's memoised, token-bearing path — a request to B carrying A's
// grant, which is a 401 at best. And the generation was one number for the whole memo, flushed
// when "the" token changed; with a table of servers there is no such number, so each entry
// remembers the generation it was built at and is rebuilt when THAT server's token moves.
/// The part of a memo key that is `Copy`. The PATH is the map's key instead, one level in, so a
/// lookup can borrow it.
///
/// `HashMap` takes its key by value in `entry`, so a flat `(u16, String, …)` key allocated a
/// `String` on every call — **hit or miss**. That is this module's hottest path: `poster_key` runs
/// once per visible tile per frame plus the hero art and logo, so at ~25–40 tiles × 60 fps it was
/// ~1,500–2,400 needless heap allocations a second on a 32-bit A53, on top of the `cstr` that
/// already built the same string. Nesting keeps the hit path allocation-free (`get` borrows a
/// `&str`) and pays for the owned key only on a real miss.
type MemoDims = (u16, c_int, c_int, bool);
struct KeyMemo {
    map: std::collections::HashMap<MemoDims, std::collections::HashMap<String, (u32, String)>>,
}
/// Entries kept before the memo is emptied wholesale. A ceiling, not a working set: a screen's
/// live keys are dozens, and this only bounds a session that browses thousands of tiles.
const MEMO_CAP: usize = 1024;

impl KeyMemo {
    /// The memo's whole rule, as a pure function of what it holds — so the two ways it can serve
    /// the wrong path are host-testable without a registry, a socket or a GL context: an entry
    /// belongs to ONE server, and it survives only as long as the token generation it was built
    /// at. `build` is called exactly when neither holds.
    fn get_or_build(
        &mut self,
        srv: u16,
        path: &str,
        w: c_int,
        h: c_int,
        png: bool,
        gen: u32,
        build: impl FnOnce() -> String,
    ) -> &str {
        let by_path = self.map.entry((srv, w, h, png)).or_default();
        // Bound the working set the same way the flat map did, one dimension bucket at a time.
        if by_path.len() > MEMO_CAP {
            by_path.clear();
        }
        // HIT: borrow the path, allocate nothing. This is the frame-rate path.
        if by_path.get(path).is_some_and(|e| e.0 == gen) {
            return &by_path[path].1;
        }
        // MISS (or a moved token): now the owned key is worth paying for.
        by_path.insert(path.to_owned(), (gen, build()));
        &by_path[path].1
    }
}
static mut KEY_MEMO: Option<KeyMemo> = None;

/// The memo is a SECOND crate global: a test that builds keys resets it, so it is never served
/// the previous test's path for the same request box.
#[cfg(test)]
fn reset_key_memo() {
    unsafe { *std::ptr::addr_of_mut!(KEY_MEMO) = None };
}


/// The store key for `(srv, path, w, h, png)`, memoised: the artwork token and the device-pixel
/// box it is decoded into. `None` for a source other than the local library or an empty path.
/// MAIN thread; borrows the memo, so the key is consumed before the next call.
fn built_key(srv: ServerId, path: &str, w: c_int, h: c_int, png: bool) -> Option<&'static str> {
    if path.is_empty() || !srv.is_local() {
        return None;
    }
    // SAFETY: main-thread only (every caller is a draw path), and the borrow is consumed by the
    // caller before the memo can be touched again; the `'static` is that discipline, not a fact.
    let memo = unsafe {
        (*std::ptr::addr_of_mut!(KEY_MEMO)).get_or_insert_with(|| KeyMemo { map: std::collections::HashMap::new() })
    };
    let n = plx_base::surface::render_scale() as c_int;
    let s = memo.get_or_build(srv.raw(), path, w, h, png, 0, || local_art_key(path, w * n, h * n));
    if s.len() > KEY_MAX {
        warn_key_refused(s.len());
        return None;
    }
    Some(s)
}

/// A refused key logs ONCE per process, with both ceilings and the length that missed them.
///
/// The latch is the whole design. This is a per-tile, per-frame path and the memo means a refusal
/// REPEATS every frame, so an unlatched log would bury `/tmp/pitv-events.log` — but silence
/// is the failure `paths.rs` was fixed for (a font fell through to DroidSans while `init_text`
/// still logged `ok=1`), and a silent refusal here is a tile that is a skeleton forever with
/// nothing in the one file an issue report is asked for. Once is enough to name the cause.
fn warn_key_refused(len: usize) {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        plx_base::eventlog::log(&format!(
            "posters: REFUSED art key of {len} bytes (slot holds {KEY_MAX}) - tile stays a skeleton"
        ));
    }
}

/// Process-wide counts of the two halves of issue #107's fix: a `P_READY` slot the render cache
/// could no longer keep resident (`PosterSource::unresident` demoting it to `P_EVICTED`), and a
/// later demand re-arming that dormant slot back to `P_WANT` (the `P_EVICTED` branch of
/// [`lookup`]). Both increment on every real transition, never on a call that finds the slot
/// already past it, so the two numbers are an exact tally, not a sample.
static RESIDENCY_LOST: AtomicU64 = AtomicU64::new(0);
static RESIDENCY_REARMED: AtomicU64 = AtomicU64::new(0);
/// A THIRD count, separate from [`RESIDENCY_LOST`]: a brand-new arrival the render cache refused
/// outright because every eviction candidate was on screen a moment ago
/// (`tex::TexCache::evict_for`, "on-screen art stays; arrivals lose"). The slot transition is
/// identical (`P_READY`/`P_DECODED` -> `P_EVICTED`, [`PosterSource::unresident`]), but nothing was
/// ever actually resident here — conflating the two under `lost=` made an over-budget idle
/// screen's diagnostic climb forever, because a refused arrival's cooldown keeps expiring and
/// retrying even once no real eviction is happening at all. Kept on this seam — not folded into
/// `tex.rs` — because that cache must stay ignorant of app-level metrics; the bool crossing it is
/// the minimal signal [`card_motion_metrics`](plx_ui::card_motion_metrics) needs.
static RESIDENCY_REFUSED: AtomicU64 = AtomicU64::new(0);

/// The interval throttle's clock, and the `(lost, rearmed, refused)` triple the LAST emitted line
/// actually reported — the rest is what lets [`log_residency_settled`] tell "nothing changed
/// since we last said so" from "the totals moved and the throttle window swallowed it".
struct ResidencyLog {
    at: Option<std::time::Instant>,
    lost: u64,
    rearmed: u64,
    refused: u64,
}
static RESIDENCY_LOG: Mutex<ResidencyLog> =
    Mutex::new(ResidencyLog { at: None, lost: 0, rearmed: 0, refused: 0 });

/// Pure half of the interval throttle in [`log_residency`] — is a transition-triggered line due,
/// given the instant of the last one actually written (`None` before the first) and now? Split
/// out so the throttle's edges (first call always due, the instant it reopens) are host-testable
/// without a real Mutex or a real elapsed second.
fn interval_due(last: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    match last {
        None => true,
        Some(t) => now.duration_since(t) >= Duration::from_secs(1),
    }
}

/// The only place these two counts reach the event log, for the same reason issue #107 itself was
/// invisible: the route heartbeat's `evicted_hot=` counts HOT evictions only (a key wanted again in
/// the same frame), so on-device verification of the fix could report "I did not see a skeleton"
/// and nothing more — never whether eviction fired at all, or whether a fired eviction was
/// followed by a working re-arm. Those are two different failures a screenshot cannot tell apart,
/// and this line is what lets a future field report on #107 tell them apart after the fact.
///
/// Throttled for the reason [`warn_key_refused`] gives for its own latch: `unresident` and the
/// re-arm branch are both per-tile, per-frame paths, and a fast scroll evicts dozens of keys a
/// second, so a line per transition would bury `/tmp/pitv-events.log`. Unlike that latch this
/// does not go silent after one line — a long session should keep producing fresh readings — so it
/// is a minimum-interval throttle (about one second, monotonic so a skewed wall clock never lies
/// about it) rather than a one-shot. The first call in a process is let through unthrottled, so a
/// short session still leaves evidence.
///
/// **The throttle bounds the LINE, not the truth**: the two atomics above are incremented on
/// every real transition regardless of whether a call here emits, so the TOTALS are always exact
/// — but a lone eviction whose re-arm lands inside the same one-second window used to leave the
/// re-arm permanently unwritten, because nothing ever asked again. A device session that captured
/// exactly `lost=1 rearmed=0` because the fix's own recovery path completed in under a second is
/// indistinguishable, on that reading alone, from a re-arm that never fires at all — which is the
/// one distinction this instrument exists to make. [`log_residency_settled`] is the other half:
/// called once a frame, it bypasses this interval outright and emits whenever the totals have
/// moved since the last line, which is what guarantees the final state always lands even when
/// every individual transition inside a burst was throttled away.
fn log_residency() {
    let lost = RESIDENCY_LOST.load(Ordering::Relaxed);
    let rearmed = RESIDENCY_REARMED.load(Ordering::Relaxed);
    let refused = RESIDENCY_REFUSED.load(Ordering::Relaxed);
    let now = std::time::Instant::now();
    let mut st = RESIDENCY_LOG.lock().unwrap_or_else(|e| e.into_inner());
    if !interval_due(st.at, now) {
        return;
    }
    st.at = Some(now);
    st.lost = lost;
    st.rearmed = rearmed;
    st.refused = refused;
    drop(st);
    plx_base::eventlog::log(&format!("posters: residency lost={lost} rearmed={rearmed} refused={refused}"));
}

/// The settle half of the instrument (see [`log_residency`]'s doc for the gap it closes): called
/// once a frame from [`begin_frame`], unconditionally, for every screen — not from
/// `PosterSource::idle`, which reaches this module only through `screens/home/mod.rs`'s prefetch
/// gate (`prefetch_armed(..) && source_idle()`) and would therefore never fire for an eviction
/// burst on Detail or a library grid, nor for a Home frame where prefetch happens to be disarmed.
/// `begin_frame` already runs first, every frame, on the main thread, regardless of screen — the
/// same unconditional seam `invalidate_due_retries` uses for its own end-of-frame housekeeping.
///
/// Emits only when the store this frame found QUIET (nothing `P_WANT`/`P_LOADING`/`P_DECODED`)
/// AND the totals moved since the last line actually written: a settle event with nothing new to
/// say costs nothing, and re-checking every frame is cheap because the caller already pays for
/// the same slot scan. Bypasses [`interval_due`] outright — a settle event is by definition rare
/// enough (bounded by bursts, not by transitions) that it does not need the throttle bursts do.
fn log_residency_settled(store_idle_this_frame: bool) {
    if !store_idle_this_frame {
        return;
    }
    let lost = RESIDENCY_LOST.load(Ordering::Relaxed);
    let rearmed = RESIDENCY_REARMED.load(Ordering::Relaxed);
    let refused = RESIDENCY_REFUSED.load(Ordering::Relaxed);
    let mut st = RESIDENCY_LOG.lock().unwrap_or_else(|e| e.into_inner());
    if st.lost == lost && st.rearmed == rearmed && st.refused == refused {
        return;
    }
    st.at = Some(std::time::Instant::now());
    st.lost = lost;
    st.rearmed = rearmed;
    st.refused = refused;
    drop(st);
    plx_base::eventlog::log(&format!("posters: residency lost={lost} rearmed={rearmed} refused={refused}"));
}

/// Test-visible read of the two original totals, so a test grades the counters through the same
/// atomics [`log_residency`] reads rather than a reimplementation of them.
#[cfg(test)]
fn residency_counts_for_test() -> (u64, u64) {
    (
        RESIDENCY_LOST.load(Ordering::Relaxed),
        RESIDENCY_REARMED.load(Ordering::Relaxed),
    )
}

/// Test-visible read of [`RESIDENCY_REFUSED`] — kept separate from
/// [`residency_counts_for_test`] rather than widening its tuple, so the many existing callers
/// that destructure `(lost, rearmed)` are untouched by this third counter.
#[cfg(test)]
fn residency_refused_for_test() -> u64 {
    RESIDENCY_REFUSED.load(Ordering::Relaxed)
}

/// Test-visible read of the `(lost, rearmed)` pair the last emitted line actually reported, so a
/// test can grade [`log_residency_settled`]'s "did the log line's content change" decision
/// directly, the same way [`residency_counts_for_test`] grades the underlying totals.
#[cfg(test)]
fn residency_last_emitted_for_test() -> (u64, u64) {
    let st = RESIDENCY_LOG.lock().unwrap_or_else(|e| e.into_inner());
    (st.lost, st.rearmed)
}

/// Force the interval throttle's clock back to "no line written yet", so a test's first call
/// through [`log_residency`] is deterministically unthrottled regardless of what an earlier test
/// (serialized the same way, through [`plx_base::testlock::serial`]) wrote a moment before. Leaves
/// the process-wide totals alone — those are graded by delta, exactly as [`reset_key_memo`]
/// leaves `plex::reset_servers_for_test` to the registry it resets.
#[cfg(test)]
fn reset_residency_log_for_test() {
    let mut st = RESIDENCY_LOG.lock().unwrap_or_else(|e| e.into_inner());
    st.at = None;
}


/// The slot a miss claims, as a PURE function of what the store looks like: the first EMPTY, else
/// the least-recently-used SETTLED (`P_READY`/`P_EVICTED`/`P_FAILED`) slot no draw has
/// touched for [`WANT_STALE_FRAMES`] frames, else the oldest stale request, else `None` —
/// "everything is either in flight or on screen; skip this request".
///
/// **A slot a draw touched LAST frame is a card on screen, exactly as one touched this frame is**
/// ([`drawn_recently`]). The per-frame stamp alone protects only the cards drawn BEFORE the miss
/// in the same frame, and a walk back (up a grid, left along a shelf) draws its arriving cards
/// first. On a slow server the off-screen slots are all queued requests, so the oldest settled
/// slot was a poster on screen, about to be drawn: it went back to its placeholder while dozens
/// of dead requests sat beside it (imgtrace `HIDDEN gap=1f cause=recycled`; 23 such recycles in
/// one Library walk of `tests/paging_link.py`, each with 38 or more stale requests in the store).
/// With nothing else to take, the miss waits: a new card stays a placeholder a few frames longer
/// rather than blank one that had its picture.
///
/// Extracted from [`lookup`] because the PREFETCH's entire safety argument is a claim about this
/// function — that a warmed slot (LRU age 0, no frame stamp) is always a more attractive victim than
/// anything a draw touched — and the store around it is not host-testable: eviction frees a GL
/// texture, and no test binary on this host links GL.
///
/// Note the second clause is what bounds a prefetch's depth, and it is sharper than eviction:
/// `P_LOADING`/`P_DECODED` are never victims (a worker holds them), so a batch of in-flight warms
/// does not merely age the store out, it can make this return `None` for a poster the user is
/// looking at — which is a tile that is never even REQUESTED, not one that arrives late.
///
/// **A STALE want is the last resort** (the third clause, [`WANT_STALE_FRAMES`]). A fast scroll
/// claims a slot for every card it passes, and on a slow server the workers fall behind: the queue
/// fills with requests for rows that left the screen before a worker reached them. Those wants are
/// settled for nobody, yet without this clause they are never victims, so the rows ON screen found
/// `store_full`. A `P_WANT` slot no worker has picked up (so there is nothing off-lock to
/// invalidate; the `gen` bump on a claim retires any stale reference) that a draw has not touched
/// for [`WANT_STALE_FRAMES`] frames is a card that scrolled away: the demand is real only while it
/// is drawn, and a later draw simply asks again. Settled slots still go first, so this never costs
/// a picture the store holds. Only DRAWN demand qualifies (`visible`): a speculative warm or a
/// lookahead entry has never been drawn and is bounded by [`warm_admissible`] instead.
fn victim(slots: &[Pslot; PT_CAP], frame: c_uint) -> Option<usize> {
    if let Some(i) = (0..PT_CAP).find(|&i| slots[i].state == P_EMPTY) {
        return Some(i);
    }
    let mut oldest = c_uint::MAX;
    let mut pick = None;
    for i in 0..PT_CAP {
        let s = &slots[i];
        if is_settled_victim(s, frame) && s.use_ < oldest {
            oldest = s.use_;
            pick = Some(i);
        }
    }
    if pick.is_some() {
        return pick;
    }
    for i in 0..PT_CAP {
        let s = &slots[i];
        if want_is_stale(s, frame) && s.use_ < oldest {
            oldest = s.use_;
            pick = Some(i);
        }
    }
    pick
}

/// How many frames a DRAWN slot (a queued request, or a settled picture) may go without a draw
/// touching it before [`victim`] may recycle it for a card that is on screen now. Long enough that a card still on screen (drawn
/// every frame) or briefly covered by a menu is never mistaken for a departed one, short enough
/// that a held scroll's dead requests (about one new card per frame at the fastest) cannot fill
/// the store's 64 slots.
const WANT_STALE_FRAMES: c_uint = 8;

/// A settled slot (nothing in flight or queued for it) that is not a card on screen: the slots
/// [`victim`] and [`ahead_victim`] may recycle without costing a request or a picture in view.
fn is_settled_victim(s: &Pslot, frame: c_uint) -> bool {
    matches!(s.state, P_READY | P_FAILED | P_EVICTED) && s.frame != frame && !drawn_recently(s, frame)
}

/// A draw asked for this slot within the last [`WANT_STALE_FRAMES`] frames: its card is on screen
/// (or left it a moment ago). The one window both rules share, so a slot is either a card in view
/// (a queued request is live, a picture is kept) or not (both are recyclable). Speculation
/// (`visible` unset: a warm, a lookahead entry nobody has drawn) never qualifies.
fn drawn_recently(s: &Pslot, frame: c_uint) -> bool {
    s.visible && frame.wrapping_sub(s.frame) < WANT_STALE_FRAMES
}

/// A request a worker has not picked up whose card has not been drawn for [`WANT_STALE_FRAMES`].
fn want_is_stale(s: &Pslot, frame: c_uint) -> bool {
    s.state == P_WANT && s.visible && !drawn_recently(s, frame)
}

/// **Lookahead** ([`Touch::Ahead`]): the slots a screen's at-rest lookahead may claim, as a PURE
/// function of the store, like [`victim`]. A lookahead warm is ordinary demand for a slot that
/// nothing is looking at yet, so it is allowed to recycle only what a picture on screen cannot
/// be: the rule, in order, is
///
/// 1. the first EMPTY slot;
/// 2. else the least-recently-used settled slot that is NOT itself a lookahead entry, was not
///    drawn this frame, and is OLDER than the [`AHEAD_PROTECT`] most recently used ordinary
///    slots. At rest nothing is stamped with the current frame, so recency is the only thing that
///    tells the cards on screen (the newest draws) from the rows that scrolled away;
/// 3. else, only once [`AHEAD_MAX`] slots are already lookahead entries, the oldest of THEM.
///    Clause 3 is what lets a jump to a new place reclaim the old place's lookahead, and is bounded
///    for the reason the next paragraph gives.
///
/// Why two lookahead warms cannot recycle each other forever (the thrash
/// `scrolled_grid_admits_only_visible_art_and_never_rewarms_hidden_rows` records for the plain
/// `Touch::Warm`, whose age-0 slot is the first victim of the next warm): a lookahead slot gets a
/// FRESH clock age, so it is the newest thing in the store and last in line for clause 2 and 3's
/// "oldest"; clause 2 never takes one at all; and clause 3 only fires at [`AHEAD_MAX`] entries,
/// which the screen's set ([`tex::AHEAD_SET_MAX`] cards, strictly fewer) cannot fill by itself —
/// so every clause-3 eviction removes an entry older than the set's newest, and the screen runs
/// out of older strangers after finitely many. A Draw is unaffected by any of this: [`victim`]
/// still sees a lookahead slot as an ordinary settled one, so a visible miss always finds a slot
/// unless every slot is in flight or a card on screen ([`drawn_recently`]).
fn ahead_victim(slots: &[Pslot; PT_CAP], frame: c_uint) -> Option<usize> {
    if let Some(i) = (0..PT_CAP).find(|&i| slots[i].state == P_EMPTY) {
        return Some(i);
    }
    let mut oldest = c_uint::MAX;
    let mut pick = None;
    for i in 0..PT_CAP {
        let s = &slots[i];
        if !s.ahead && is_settled_victim(s, frame) && s.use_ < oldest
            // `newer` counts every ordinary slot younger than this one, in flight or settled.
            && slots.iter().filter(|o| !o.ahead && o.state != P_EMPTY && o.use_ > s.use_).count()
                >= AHEAD_PROTECT
        {
            oldest = s.use_;
            pick = Some(i);
        }
    }
    if pick.is_some() || slots.iter().filter(|s| s.ahead).count() < AHEAD_MAX {
        return pick;
    }
    for i in 0..PT_CAP {
        let s = &slots[i];
        if s.ahead && is_settled_victim(s, frame) && s.use_ < oldest {
            oldest = s.use_;
            pick = Some(i);
        }
    }
    pick
}

/// The most recently used ordinary slots a lookahead warm may never recycle: the cards on screen.
///
/// This is the protection the cards on screen have against a lookahead claim that does not rest
/// on a frame stamp. The claim runs in `prepare`, before any draw of the frame, so no slot carries
/// the current frame's stamp, and [`drawn_recently`] covers a card only for the few loop turns
/// after its last draw, which a screen at rest can outlast; recency does the work, on the
/// premise that the last frame drew the cards on screen and so left them the newest ordinary
/// slots. That holds only while the protected count covers every card a screen paints at once:
/// [`tex::ON_SCREEN_ART_MAX`] (the Library: six columns by five partly visible rows, 30, pinned to
/// the layout in `screens/src/library/lookahead_tests.rs`), which 32 keeps out of reach while
/// leaving [`AHEAD_MAX`] slots for the lookahead itself. The assertion below fails the build if
/// either number moves past the other.
const AHEAD_PROTECT: usize = 32;
const _: () = assert!(AHEAD_PROTECT >= tex::ON_SCREEN_ART_MAX);
/// The most slots lookahead entries may hold. `AHEAD_PROTECT + AHEAD_MAX == PT_CAP`, and
/// [`tex::AHEAD_SET_MAX`] (what a screen may ask for at once) stays below it.
const AHEAD_MAX: usize = PT_CAP - AHEAD_PROTECT;
const _: () = assert!(tex::AHEAD_SET_MAX < AHEAD_MAX);

/// What one store probe found: the slot's key once its pixels have been handed to the render
/// cache (READY), else `None` while the slot is empty, in flight, or failed. The texture and its
/// decoded size are the cache's answer for that key.
type Hit = Option<PosterKey>;

/// MAIN thread. The one store lookup behind [`poster_get`], [`poster_get_wh`] and [`poster_warm`]:
/// hit → the READY texture + its size; miss → claim a slot and enqueue the fetch, IF the request is
/// admitted. Three things decide, and `touch` is only the first: `touch` itself (LRU bookkeeping on
/// every hit, and whether a P_EVICTED slot is eligible to recover at all), the speculation bound
/// [`warm_admissible`] for a Warm, and the drawn card's placement scope
/// ([`plx_ui::card_motion::declines_request`]) for a Draw. An unknown card placement declines new
/// work (a moving one too, but only under the dev trigger `pitv-cardspeed`); featured images
/// outside that scope remain eligible.
/// A refusal claims no source slot; a follow-up present samples placement again.
fn lookup(srv: ServerId, key_s: &str, touch: Touch) -> (Hit, Warm) {
    // The array's own precondition, checked before a slot can be claimed for a key that could
    // never match its probe again. `Warm::Known` (not `Full`) so a prefetch loop walks on to the
    // next candidate instead of retiring this frame's one warm here.
    if !is_fetchable(key_s) {
        return (None, Warm::Known);
    }
    // A scoped card DRAW may not START work with unknown placement (or, under the dev trigger
    // `pitv-cardspeed`, fast placement), at EVERY point work begins — the fresh miss far
    // below, but equally the two re-arm transitions inside the matching-slot loop. Those matter
    // more than the miss rather than less: a scroll is what evicts textures, so scrolling back
    // across the same rows finds P_EVICTED slots, and re-arming each one is a full fetch, decode
    // and upload that the gate would never see if it only guarded the miss. A HIT is deliberately untouched: art that is
    // already resident still draws and still takes its LRU touch, so declining never blanks a
    // tile that had its picture.
    // (`declines_request` is always false in a frame dump: a frozen clock never leaves `Unknown`.)
    let decline = touch == Touch::Draw && plx_ui::card_motion::declines_request();
    let mut g = store();
    let cache_gen = plx_platform::imgcache::generation();
    // hit?
    for i in 0..PT_CAP {
        if g.slots[i].cache_gen == cache_gen && same_art(&g.slots[i], srv, key_s.as_bytes()) {
            if touch == Touch::Draw {
                g.clock = g.clock.wrapping_add(1);
                let (c, f) = (g.clock, g.frame);
                g.slots[i].use_ = c;
                g.slots[i].frame = f;
                // Promotion is monotone: a warm never demotes a key a draw is waiting on. If it
                // is still queued, the next worker rescan will now choose it before speculation.
                g.slots[i].visible = true;
                g.slots[i].ahead = false;
                if g.slots[i].state == P_WANT { CV.notify_one(); }
            }
            // Render-cache pressure is not source demand: its callback parks the slot in EVICTED,
            // and mere retention in the source LRU schedules no work. Only a Draw probe may even
            // consider re-arming an existing EVICTED slot; Warm leaves it dormant unconditionally,
            // below, before either of the DRAW-only mechanisms that follow ever run. This prevents
            // speculative resurrection of that slot. EVICTED is reached several ways — a rejected
            // decode, an off-screen (never-drawn) resident released under count/byte pressure, or a
            // brand-new arrival refused outright because on-screen art was in the way
            // (`tex::TexCache::evict_for`, "on-screen art stays; arrivals lose") — and
            // [`PosterSource::unresident`]'s `refused` flag is the only place that still
            // distinguishes the last of those from the other two. What the cooldown gate right
            // after this one bounds is the RETRY side of a refusal under sustained pressure — a key
            // whose cooldown keeps clearing only for its re-upload to be refused again immediately
            // has nothing that stops the retry loop by itself (see `evict_was_rapid`'s doc): a
            // key's first eviction or refusal always re-arms on its very next draw (no cooldown was
            // set for it), but one that keeps recurring rapidly waits out a bounded backoff instead
            // of retrying on every single draw.
            if g.slots[i].state == P_EVICTED {
                if touch != Touch::Draw {
                    return (None, Warm::Known);
                }
                let now = crate::app::clock::now();
                if !evict_cooldown_due(g.slots[i].evict_cooldown_until, now) {
                    // Still cooling down from a rapid re-eviction. The demand is real and is not
                    // lost — the next probe after the cooldown clears re-arms exactly as usual —
                    // but honoring THIS one would undo the rate limit the cooldown exists to
                    // enforce. It does not stop the cycle: under sustained pressure the key can be
                    // evicted and cooled down again the moment it lands, indefinitely — it only
                    // paces how often that cycle is allowed to turn.
                    trace::outcome("evict_cooldown");
                    return (None, Warm::Known);
                }
                if decline {
                    plx_ui::card_motion::deferred();
                    #[cfg(feature = "devtriggers")]
                    plx_ui::card_motion_metrics::refused(plx_ui::card_motion_metrics::Refused::Evicted);
                    // The same deferral the cooldown just made, for the same reason: the slot
                    // stays EVICTED and the first draw at a settled speed re-arms it as usual.
                    trace::outcome("declined_evicted");
                    return (None, Warm::Known);
                }
                #[cfg(feature = "devtriggers")]
                {
                    plx_ui::card_motion_metrics::request();
                    plx_ui::card_motion_metrics::rearmed();
                }
                g.slots[i].state = P_WANT;
                g.slots[i].evict_wake_sent = false;
                let gen = g.slots[i].gen;
                drop(g);
                trace::claim(trace::current(), i, gen);
                RESIDENCY_REARMED.fetch_add(1, Ordering::Relaxed);
                log_residency();
                CV.notify_one();
                return (None, Warm::Known);
            }
            let hit = (g.slots[i].state == P_READY).then_some(PosterKey(i as u32));
            trace::outcome(match g.slots[i].state { P_READY => "ready", P_WANT => "queued", P_LOADING => "loading", P_DECODED => "decoded", P_FAILED => "failed", _ => "other" });
            return (hit, Warm::Known);
        }
    }
    // The same picture held under an older cache generation: the hit loop above could not match
    // it, so whatever this draw gets next it lost that picture for THIS reason.
    if trace::armed() && g.slots.iter().any(|s| same_art(s, srv, key_s.as_bytes())) {
        trace::lost_id(trace::current(), "cache_gen");
    }
    // Admit speculation only into a quiet visible queue, and never enough of it to occupy both
    // workers. `Full` tells the caller this frame's one prefetch attempt is spent without claiming
    // a slot.
    if touch != Touch::Draw && !warm_admissible(&g.slots) {
        return (None, Warm::Full);
    }
    // A card MISS with unknown placement (or, under the dev trigger `pitv-cardspeed`, fast
    // on-screen motion) claims no slot and starts no acquisition. That tile draws its placeholder
    // and asks again next frame. Other cards, featured images, and previously admitted work remain
    // eligible; this gate defers only the drawn card's new demand until its placement is known
    // (and, under the trigger, sufficiently still). The shipping default declines only the
    // unknown placement: a moving card claims its slot at once.
    //
    // **History: why the moving-card decline was once outright rather than paced.** Every weaker
    // bound was measured on the dev set against a 1000-movie mock library (`fps:library-scroll`,
    // panel off, two runs each, dropped frames after warmup). These numbers predate PR #563's
    // offscreen warm-up and the at-rest lookahead (`screens/src/library/lookahead.rs`), and the
    // shipping default has since dropped the moving-card decline:
    //
    // |                                                     | drops      | median fps |
    // |-----------------------------------------------------|------------|------------|
    // | a9d4e8a2, no gate                                    | 90, 97     | 55         |
    // | slot + cache caps 64 -> 160                          | 226, 231   | 50         |
    // | rate-limit the uploads after decode                  | 172, 156   | 53, 55     |
    // | allow ONE visible request in flight while scrolling  | 204, 196   | 55, 54     |
    // | **decline the request outright**                     | **21, 23** | **60**     |
    //
    // One in flight is the informative row: it paced the pipeline perfectly — `budget=` read
    // 15-18 admitted and ZERO refused, against the ungated 15-18 admitted and 11-15 refused —
    // and still cost twice the baseline's dropped frames. Pacing the work does not make it
    // affordable. The two downstream bounds were worse than doing nothing at all, because by
    // then the fetch and the decode are already paid for and the refused pixels only accumulate
    // in `TexCache::pending` at 375 KB each. The request is the last point at which this work
    // can still be declined for free.
    //
    // **Why the default no longer declines.** Remeasured on the television with the lookahead in
    // place (1000-movie mock, panel off, two runs each; the old default is `pitv-lookahead=0
    // pitv-cardspeed=120`): placeholder tile-frames seen — `library-burst` 28,969 / 28,080
    // old default, 1,759 / 1,761 new; `library-hold` 35,850 / 35,592 old, 4,280 / 3,902 new —
    // against frames over 33 ms of 7 / 16 old and 30 / 10 new in `library-burst`, and of 3 / 8 old
    // and 18 / 19 new in `library-hold`. `library-scroll` holds a 60 fps median on both, with a
    // late-upload share of 6% / 14%. The cost of a request per passing card is queue depth on a
    // slow server, which [`victim`] (a stale request is recyclable) and [`next_wanted`] (newest
    // draw first) bound.
    if decline {
        plx_ui::card_motion::deferred();
        #[cfg(feature = "devtriggers")]
        plx_ui::card_motion_metrics::refused(plx_ui::card_motion_metrics::Refused::New);
        trace::outcome("declined_new");
        return (None, Warm::Full);
    }
    // miss: prefer EMPTY, else LRU-evict a settled slot no card on screen is using
    let chosen = if touch == Touch::Ahead { ahead_victim(&g.slots, g.frame) } else { victim(&g.slots, g.frame) };
    let idx = match chosen {
        Some(i) => i,
        None => { trace::outcome("store_full"); return (None, Warm::Full) } // all visible: skip
    };
    let (was_ready, old_px) = (g.slots[idx].state == P_READY, g.slots[idx].px);
    if trace::armed() {
        if g.slots[idx].state != P_EMPTY {
            trace::lost(idx, g.slots[idx].gen, "recycled");
        }
    }
    let (use_, frame) = match touch {
        Touch::Draw => {
            g.clock = g.clock.wrapping_add(1);
            (g.clock, g.frame)
        }
        // age 0 = older than everything a draw has touched, so a warmed slot is always the FIRST
        // victim of the next miss; frame-1 is by construction never the current frame, so it never
        // carries evict-protection — and unlike a literal 0 it does not depend on `poster_pump`
        // having bumped the frame counter yet (a screen's update runs BEFORE the pump).
        Touch::Warm => (0, g.frame.wrapping_sub(1)),
        // A fresh age (never the next victim of a Warm or a lookahead claim) with the same
        // unprotected frame stamp: a Draw this frame still outranks it.
        Touch::Ahead => {
            g.clock = g.clock.wrapping_add(1);
            (g.clock, g.frame.wrapping_sub(1))
        }
    };
    {
        let s = &mut g.slots[idx];
        s.px = 0;
        s.gen = s.gen.wrapping_add(1);
        s.cache_gen = cache_gen;
        set_key(s, key_s);
        s.srv = srv; // captured HERE, on the main thread — the worker asks no one which server
        s.state = P_WANT;
        s.visible = touch == Touch::Draw;
        s.ahead = touch == Touch::Ahead;
        s.use_ = use_;
        s.frame = frame;
        s.pw = 0;
        s.ph = 0;
        // A recycled slot is a DIFFERENT key's identity now — the eviction-thrash history above
        // belongs to whatever image used to live here, not to this one.
        s.evicted_at = None;
        s.evict_attempts = 0;
        s.evict_cooldown_until = None;
        s.evict_wake_sent = false;
    }
    let claimed_gen = g.slots[idx].gen;
    drop(g);
    trace::claim(trace::current(), idx, claimed_gen);
    // free the evicted resources off-lock (this is the GL/main thread): the cache's texture for
    // the recycled slot, and pixels a worker decoded that nobody drained
    if was_ready {
        tex::free(PosterKey(idx as u32), &mut GfxUploader);
    }
    if old_px != 0 {
        img::img_free(old_px as *mut c_uchar);
    }
    #[cfg(feature = "devtriggers")]
    plx_ui::card_motion_metrics::request();
    CV.notify_one();
    (None, Warm::Claimed)
}

/// The box a clearLogo is decoded into: wide enough for the hero rung's widest wordmark
/// (`theme::logo::HERO_AREA` over its height floor), so the hero never draws it upscaled.
const LOGO_BOX: (c_int, c_int) = (800, 268);

/// The [`tex::Source`] this module is to the library: one value, installed at [`init`].
struct PosterSource;
static SOURCE: PosterSource = PosterSource;

impl tex::Source for PosterSource {
    fn probe(&self, srv: u16, path: &str, w: i32, h: i32, png: bool) -> Option<PosterKey> {
        let sid = ServerId::from_raw(srv);
        let key = built_key(sid, path, w, h, png)?;
        if !trace::armed() {
            return lookup(sid, key, Touch::Draw).0;
        }
        let id = trace::draw_id(srv, path, w, h, png);
        let kid = trace::key_id(srv, key.as_bytes());
        trace::set_current(id);
        let hit = lookup(sid, key, Touch::Draw).0;
        let gate = match plx_ui::card_motion::verdict() {
            Some(plx_ui::card_motion::Verdict::Unknown) => trace::Gate::Unknown,
            Some(plx_ui::card_motion::Verdict::Moving) => trace::Gate::Moving,
            _ => trace::Gate::Open,
        };
        trace::probe(id, kid, hit.is_some_and(tex::resident), gate);
        trace::set_current(0);
        hit
    }
    /// The prefetch: start the fetch, take no key, take no LRU protection. Two deliberate
    /// differences from a draw's probe, and together they are why a prefetch can share a 64-slot
    /// store with everything on screen: it never stamps `frame` (no evict-protection) and never
    /// bumps `use_` (LRU age 0, permanently the first victim). A key that cannot be built answers
    /// `Known`, so a prefetch loop walks on to the next candidate instead of retiring this
    /// frame's one warm on a key that can never resolve.
    fn warm(&self, srv: u16, path: &str, w: i32, h: i32, png: bool) -> Warm {
        let srv = ServerId::from_raw(srv);
        match built_key(srv, path, w, h, png) {
            Some(k) => {
                if trace::armed() { trace::set_current(trace::draw_id(srv.raw(), path, w, h, png)); }
                let r = lookup(srv, k, Touch::Warm).1;
                if trace::armed() { trace::set_current(0); }
                r
            }
            None => Warm::Known,
        }
    }
    /// The at-rest lookahead: a [`warm`](Self::warm) that survives. Same key, same admission
    /// ([`warm_admissible`]), same dormancy for an EVICTED slot — only the slot it claims differs
    /// ([`ahead_victim`]), which is what keeps a screenful of lookahead from evicting itself.
    fn warm_ahead(&self, srv: u16, path: &str, w: i32, h: i32, png: bool) -> Warm {
        let srv = ServerId::from_raw(srv);
        built_key(srv, path, w, h, png).map_or(Warm::Known, |k| lookup(srv, k, Touch::Ahead).1)
    }
    /// A local title's clearLogo: `rk` is its artwork token (`local::Entry::logo`), decoded into
    /// the one [`LOGO_BOX`] as a PNG so its alpha survives. An empty token is a title with none.
    fn logo(&self, srv: u16, rk: &str) -> Option<PosterKey> {
        self.probe(srv, rk, LOGO_BOX.0, LOGO_BOX.1, true)
    }
    fn logo_warm(&self, srv: u16, rk: &str) -> Warm {
        self.warm(srv, rk, LOGO_BOX.0, LOGO_BOX.1, true)
    }
    fn logo_failed(&self, srv: u16, rk: &str) -> bool {
        let sid = ServerId::from_raw(srv);
        let Some(key) = built_key(sid, rk, LOGO_BOX.0, LOGO_BOX.1, true) else { return true };
        let g = store();
        let gen = plx_platform::imgcache::generation();
        g.slots.iter().any(|s| s.cache_gen == gen && same_art(s, sid, key.as_bytes()) && s.state == P_FAILED)
    }
    fn unresident(&self, key: PosterKey, refused: bool) {
        let mut g = store();
        let mut transitioned = false;
        if let Some(s) = g.slots.get_mut(key.0 as usize) {
            // A delivered decode or READY entry depends on render-cache residency. The cache
            // calls this synchronously for a rejected decoded result, pressure eviction, or a
            // brand-new arrival refused outright (`refused`), before application code can probe
            // or recycle the key. Thus the source never answers READY for a key the cache could
            // not make resident.
            if matches!(s.state, P_READY | P_DECODED) {
                s.state = P_EVICTED;
                transitioned = true;
                trace::lost(key.0 as usize, s.gen, if refused { "refused" } else { "evicted" });
                // The thrash guard's bookkeeping (see `evict_was_rapid`'s doc): a rapid re-eviction
                // (this key was evicted before, inside the thrash window) escalates the backoff
                // `lookup`'s P_EVICTED branch will honor on the next probe; an isolated one leaves
                // no cooldown at all, so the very next Draw probe re-arms it as before. A refused
                // arrival gets the identical cooldown treatment — its slot was marked `P_READY` by
                // `drain_decoded` before the cache ever saw its pixels (issue: the render cache's
                // admission decision runs strictly after that), so from here on it needs exactly
                // the same pacing a genuine eviction does, or the very next Draw probe would retry
                // the identical rejected upload every single frame.
                let now = crate::app::clock::now();
                s.evict_attempts = if evict_was_rapid(s.evicted_at, now) {
                    s.evict_attempts.saturating_add(1)
                } else {
                    0
                };
                s.evicted_at = Some(now);
                s.evict_cooldown_until = (s.evict_attempts > 0)
                    .then(|| now.wrapping_add(evict_backoff(s.evict_attempts).as_millis() as u32));
                // A fresh eviction is a fresh deadline (or none at all) to wake for — whatever an
                // earlier cooldown's expiry already latched no longer applies.
                s.evict_wake_sent = false;
            }
        }
        drop(g);
        if transitioned {
            // Same slot transition either way (`P_READY`/`P_DECODED` -> `P_EVICTED`), but a
            // REFUSED arrival never held a texture at all: it is a separate counter and a separate
            // field in the log line, not a residency LOSS.
            if refused {
                RESIDENCY_REFUSED.fetch_add(1, Ordering::Relaxed);
            } else {
                RESIDENCY_LOST.fetch_add(1, Ordering::Relaxed);
                #[cfg(feature = "devtriggers")]
                plx_ui::card_motion_metrics::evicted();
            }
            log_residency();
        }
    }
    fn idle(&self) -> bool {
        store_idle()
    }
}

/// Pure half of [`store_idle`] — split so the gate is host-testable without mutating the store
/// singleton.
fn idle_of(slots: &[Pslot; PT_CAP]) -> bool {
    !slots
        .iter()
        .any(|s| matches!(s.state, P_WANT | P_LOADING | P_DECODED))
}

/// The next queued slot a worker claims. Kept pure so queue ordering is host-testable.
fn next_wanted(slots: &[Pslot; PT_CAP]) -> Option<usize> {
    #[cfg(test)]
    if std::env::var_os("PLX_TEST_FIFO_POSTER_QUEUE").is_some() {
        return (0..PT_CAP).find(|&i| slots[i].state == P_WANT);
    }
    // Drawn demand first, and of that the MOST RECENTLY drawn: on a slow server a scroll leaves
    // requests for rows that are gone behind the ones on screen, and a worker that served them in
    // slot order would spend its seconds on pictures nobody sees. Within the newest frame the
    // lowest LRU age is the card drawn first, i.e. reading order. Speculation comes last.
    (0..PT_CAP)
        .filter(|&i| slots[i].state == P_WANT && slots[i].visible)
        .max_by_key(|&i| (slots[i].frame, std::cmp::Reverse(slots[i].use_)))
        .or_else(|| (0..PT_CAP).find(|&i| slots[i].state == P_WANT))
}

#[inline]
fn outstanding(s: &Pslot) -> bool {
    matches!(s.state, P_WANT | P_LOADING | P_DECODED)
}

/// Whether one speculative claim can enter without starving the visible set.
fn warm_admissible(slots: &[Pslot; PT_CAP]) -> bool {
    if slots.iter().any(|s| outstanding(s) && s.visible) {
        return false;
    }
    slots.iter().filter(|s| outstanding(s) && !s.visible).count() < PREFETCH_OUTSTANDING_MAX
}

/// MAIN thread. Is the store QUIET — nothing wanted, fetching, or waiting to upload? Hero warming
/// uses this conservative whole-pipeline gate. Other speculative warms (an up-next episode
/// thumbnail, a hero logo) are bounded separately by [`warm_admissible`] and ordered by
/// [`next_wanted`].
///
/// Cost: one 64-slot scan per frame under the mutex — the draw already does dozens.
fn store_idle() -> bool {
    idle_of(&store().slots)
}

// (The old standalone `poster_wh` — a second lock + 64-slot key scan to read two ints about a slot
// the caller had just probed — is gone: `lookup` hands the size back with the texture, so a
// cover-fitted tile now costs exactly what a stretched one does. Both of its callers, `logo_src` and
// `ui::widgets::resolve_tex_wh`, go through `poster_get_wh`.)

/// MAIN thread, once per frame, first: a new frame — nothing is "touched" yet (evict-protection
/// is per frame, see [`victim`]). Also where the residency instrument's settle snapshot is
/// checked (see [`log_residency_settled`]'s doc for why THIS seam and not `PosterSource::idle`):
/// the slot scan below is the same one [`idle_of`] would run, so this is the store's one
/// once-a-frame, screen-agnostic read of whether it is quiet.
pub(crate) fn begin_frame() {
    trace::begin_frame();
    let now = crate::app::clock::now();
    let mut g = store();
    g.frame = g.frame.wrapping_add(1);
    invalidate_due_evictions(&mut g.slots, now);
    let settled = idle_of(&g.slots);
    drop(g);
    log_residency_settled(settled);
}

/// MAIN thread, once per frame (§3.3 step 3, the adapter's results): every slot a worker has
/// DECODED hands its pixels to the render cache as one `PosterReady` and becomes READY. No GL
/// here — the cache uploads in PREPARE ([`prepare`]). The pixels are copied once out of the
/// decoder's C allocation into the owned `Decoded` (a 250x375 poster is ~375 KB, tens of
/// microseconds) so the library owns what it uploads.
pub(crate) fn drain_decoded() {
    loop {
        let (idx, px, w, h, current) = {
            let mut g = store();
            let Some(i) = (0..PT_CAP).find(|&i| g.slots[i].state == P_DECODED) else {
                break;
            };
            let s = &mut g.slots[i];
            let (px, w, h) = (s.px, s.pw, s.ph);
            s.px = 0;
            trace::handoff(i, s.gen);
            let current = s.cache_gen == plx_platform::imgcache::generation();
            // Retain the decoded-byte charge until tex::accept publishes its pending bytes.
            // A brief double charge is safe; a gap would let both workers over-admit decodes.
            if !current { s.state = P_FAILED; }
            (i, px, w, h, current)
        };
        if !current {
            if px != 0 { img::img_free(px as *mut c_uchar); }
            continue;
        }
        let key = PosterKey(idx as u32);
        let result = if px != 0 && w > 0 && h > 0 && w <= u16::MAX as c_int && h <= u16::MAX as c_int {
            let n = (w as usize) * (h as usize) * 4;
            // SAFETY: the worker decoded exactly w*h RGBA bytes at `px` (img::img_decode_rgba's
            // contract) and handed the pointer over under the lock; it is freed right below.
            let rgba: Box<[u8]> = unsafe { std::slice::from_raw_parts(px as *const u8, n) }.into();
            Ok(Decoded {
                w: w as u16,
                h: h as u16,
                rgba,
            })
        } else {
            Err(PosterError::Decode)
        };
        if px != 0 {
            img::img_free(px as *mut c_uchar);
        }
        tex::accept(PosterReady { key, result });
        let mut g = store();
        // Rejection may synchronously notify unresident during accept. Preserve that verdict.
        if g.slots[idx].state == P_DECODED { g.slots[idx].state = P_READY; }
    }
}

/// The GL half behind the cache, on the main thread: `img::img_upload_rgba` (a synchronous
/// glTexImage2D, counted for the frame-drop detector), `gfx::warm_tex` (resident NOW rather than
/// on the next draw that samples it, drawn into an offscreen target so it never takes the
/// back-buffer wait — see that fn for the 116 ms frame it moves out of the draw)
/// and `gfx::delete_tex`.
pub(crate) struct GfxUploader;

impl Uploader for GfxUploader {
    fn upload(&mut self, d: &Decoded) -> Tex {
        let id = img::img_upload_rgba(d.rgba.as_ptr(), d.w as c_int, d.h as c_int);
        UP_CT.fetch_add(1, Ordering::Relaxed);
        #[cfg(feature = "devtriggers")]
        plx_ui::card_motion_metrics::upload();
        UP_PX.fetch_add((d.w as u64) * (d.h as u64), Ordering::Relaxed);
        Tex {
            id,
            w: d.w,
            h: d.h,
        }
    }
    fn warm(&mut self, t: Tex) {
        if t.id != 0 {
            plx_gfx::gfx::warm_tex(t.id);
        }
    }
    fn free(&mut self, t: Tex) {
        if t.id != 0 {
            plx_gfx::gfx::delete_tex(t.id);
        }
    }
}

/// MAIN/GL thread, once per PRESENTING frame (§3.3 step 9): upload what the cache holds pending,
/// under the frame budget — the `Poster` class for an ordinary poster, the solo `Residency` class
/// for a backdrop or a hero logo (`ui::tex`'s `RESIDENCY_BYTES`).
///
/// **The frame this runs on has already been decided to present**, because the budget's
/// queued-work term is the second half of that decision (`app/run.rs`, §3.3 step 8). So what
/// `idle::invalidate` marks here is not this frame but the NEXT one, and it is still worth its
/// frame: a landed texture is DISCRETE damage, which is exactly what `idle::present_dirty`'s
/// readers — the glass hosts deciding whether to re-snapshot the page under them — are asking
/// about, and no spring reports it. The cache's own `Provenance::Resource` note is the same
/// statement to the phase-2 `Present` machine.
pub(crate) fn prepare(
    b: &mut plx_ui::frame::Budget,
    present: &mut plx_machine::machine::PresentHandle<'_>,
    now_us: impl Fn() -> u64,
) -> usize {
    let n = tex::prepare(b, &mut GfxUploader, present, now_us);
    if n > 0 {
        CV.notify_all();
        plx_machine::idle::invalidate();
    }
    n
}

/// A key from a source other than the local library never loads. Logged ONCE per process: a grid
/// claims dozens of slots at once, and a log that scrolls itself away is as useless as a silent
/// one.
fn warn_not_local(srv: ServerId) {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        plx_base::eventlog::log(&format!(
            "posters: art source {} is not the local library - its tiles draw no picture (further ones are silent)",
            srv.raw()
        ));
    }
}

/// The store key of a local artwork token decoded into a `w`×`h` box (device pixels).
fn local_art_key(token: &str, w: c_int, h: c_int) -> String {
    format!("local:{w}x{h}:{token}")
}

/// Read and decode the image a [`local_art_key`] names, fitted to its box. Off the frame thread.
fn load_local_art(key: &str) -> Option<(u32, u32, Vec<u8>)> {
    let rest = key.strip_prefix("local:")?;
    let (bx, token) = rest.split_once(':')?;
    let (bw, bh) = bx.split_once('x')?;
    let path = plx_data::local::art_path(token)?;
    let bytes = std::fs::read(&path).ok()?;
    img::img_decode_fit(&bytes, bw.parse().ok()?, bh.parse().ok()?)
}

/// BACKGROUND worker: claim a request, read disk or fetch and decode off-lock, publish pixels.
/// RAM/GPU residency stays bounded independently of the number of persistent images.
fn poster_worker() {
    loop {
        let (idx, key_s, srv, gen, cache_gen) = {
            let mut g = store();
            let idx = loop {
                if g.quit { return; }
                note_backlog(&g.slots);
                if decode_admitted(&g.slots, tex::pending_bytes()) {
                    if let Some(i) = next_wanted(&g.slots) { break i; }
                }
                g = CV.wait_timeout(g, Duration::from_millis(50))
                    .map(|(guard, _)| guard).unwrap_or_else(|e| e.into_inner().0);
            };
            let s = &mut g.slots[idx];
            s.state = P_LOADING;
            (idx, String::from_utf8_lossy(key_bytes(s)).into_owned(), s.srv, s.gen, s.cache_gen)
        };
        trace::worker_start(idx, gen);
        let (mut w, mut h) = (0, 0);
        let mut px = std::ptr::null_mut();
        if srv.is_local() {
            if let Some((lw, lh, rgba)) = load_local_art(&key_s) {
                px = img::img_malloc_copy(&rgba, || format!("{lw}x{lh} local artwork"));
                (w, h) = (lw as c_int, lh as c_int);
            }
        } else {
            warn_not_local(srv);
        }
        // A file that is missing or does not decode is final: retrying cannot change it.
        let accepted = {
            let mut g = store();
            let s = &mut g.slots[idx];
            if s.gen == gen && s.state == P_LOADING {
                if cache_gen == plx_platform::imgcache::generation() && !px.is_null() {
                    s.px = px as usize;
                    s.pw = w;
                    s.ph = h;
                    s.state = P_DECODED;
                    trace::decoded(idx, gen);
                    true
                } else {
                    if plx_gfx::dump::armed() { note_dump_failure(key_bytes(s), false); }
                    s.state = P_FAILED;
                    trace::lost(idx, gen, "failed");
                    false
                }
            } else { false }
        };
        if !accepted && !px.is_null() { img::img_free(px); }
    }
}

pub(crate) fn init() {
    tex::install(&SOURCE);
    {
        let mut g = store();
        g.quit = false;
        g.slots = [Pslot::ZERO; PT_CAP];
        g.workers.clear();
    }
    // filter_map, not map: a refused worker is one fewer decoder, not a dead app. Artwork degrades
    // to whatever the survivors can decode (and to nothing at all if both are refused).
    let handles: Vec<JoinHandle<()>> = (0..2)
        .filter_map(|_| plx_base::task::spawn("poster", poster_worker))
        .collect();
    store().workers = handles;
}

pub(crate) fn shutdown() {
    {
        let mut g = store();
        g.quit = true;
    }
    CV.notify_all();
    let handles = std::mem::take(&mut store().workers);
    for h in handles {
        plx_base::task::join("poster", h);
    }
    // free pending decodes, then every resident texture (main thread for GL); workers are joined
    let mut to_free = Vec::with_capacity(PT_CAP);
    {
        let mut g = store();
        for i in 0..PT_CAP {
            to_free.push(g.slots[i].px);
            g.slots[i] = Pslot::ZERO;
        }
    }
    for px in to_free {
        if px != 0 {
            img::img_free(px as *mut c_uchar);
        }
    }
    tex::shutdown(&mut GfxUploader);
}

#[cfg(test)]
mod tests {
    //! The pure half of the store: which slot a miss claims, whether the store is quiet, and what
    //! [`poster_key`] builds.
    //!
    //! The LRU tests are ordinary parallel ones — each drives a LOCAL `[Pslot; PT_CAP]` array
    //! rather than the `STORE` singleton, deliberately: the singleton's eviction path frees a GL
    //! texture and no test binary on this host links GL. That boundary is exactly why [`victim`]
    //! and [`idle_of`] were split out of [`lookup`]/[`store_idle`] in the first place.
    //!
    //! The key-building tests are the exception and take [`plx_base::testlock::serial`]: they need a
    //! server in the registry, which is a crate global. They still touch no socket and no GL —
    //! `poster_key` only formats a string.
    use super::*;

    /// The LRU's two clauses, in order: an EMPTY slot is always preferred (a fresh store must fill
    /// before it evicts anything), and only once there is none does the oldest SETTLED slot go.
    #[test]
    fn victim_prefers_empty_then_oldest_settled() {
        let empty = [Pslot::ZERO; PT_CAP];
        assert_eq!(
            victim(&empty, 7),
            Some(0),
            "an untouched store fills from the front"
        );

        let mut one_hole = [ready(100, 1); PT_CAP];
        one_hole[40] = Pslot::ZERO;
        assert_eq!(
            victim(&one_hole, 7),
            Some(40),
            "an empty slot beats every settled one"
        );

        let mut full = [ready(100, 1); PT_CAP];
        full[17].use_ = 3; // the oldest
        full[52].use_ = 9;
        assert_eq!(
            victim(&full, 7),
            Some(17),
            "with nothing empty, the least recently used goes"
        );

        // FAILED is settled too — a missing file must not pin a slot forever.
        let mut failed = [ready(100, 1); PT_CAP];
        failed[8] = Pslot {
            state: P_FAILED,
            use_: 1,
            frame: 1,
            ..Pslot::ZERO
        };
        assert_eq!(victim(&failed, 7), Some(8));
    }


    #[test]
    fn demand_waits_for_decoded_bytes_in_both_queues_to_drain() {
        let mut slots = [Pslot::ZERO; PT_CAP];
        assert!(decode_admitted(&slots, 0));
        slots[0].state = P_DECODED;
        slots[0].pw = 1024;
        slots[0].ph = 1024;
        assert!(decode_admitted(&slots, 0));
        assert!(!decode_admitted(&slots, 4 * 1024 * 1024));
        slots[1] = slots[0];
        assert!(!decode_admitted(&slots, 0));
        slots[0].state = P_READY;
        assert!(decode_admitted(&slots, 0));
        assert!(!decode_admitted(&slots, DECODE_BACKLOG_MAX));
    }


    /// The test lock and a clean key memo, the memo reset again on the way out.
    struct Fresh(#[allow(dead_code)] plx_base::testlock::Serial);
    impl Drop for Fresh {
        fn drop(&mut self) {
            reset_key_memo();
        }
    }
    /// The local library's art source, under the test lock.
    fn one_server() -> (Fresh, ServerId, &'static str) {
        let g = plx_base::testlock::serial();
        reset_key_memo();
        (Fresh(g), ServerId::from_raw(plx_data::local::ART_SRV), "")
    }

    /// Build a key through the real [`built_key`] — the same call `lookup` makes for a draw —
    /// so these grade the string a draw actually receives, not a reimplementation of it.
    fn key_for(srv: ServerId, src: &str, w: c_int, h: c_int, png: c_int) -> String {
        built_key(srv, src, w, h, png != 0).unwrap_or("").to_owned()
    }



    /// **A tag-shaped `/hubs/search` `collection` row (a request without `includeCollections=1`)
    /// carries no `thumb` at all** (verified live — no
    /// `ratingKey` either; only `key` and a tag `id`), so an empty source stops being a rarity and
    /// becomes a whole shelf of them. It must produce NO request: `…&url=&X-Plex-Token=…` is a
    /// `404 text/html` from this server (measured), and every one of them would burn a slot as
    /// `P_FAILED` until the LRU walked back to it.
    ///
    /// The empty key is the store's existing "nothing to fetch" convention — the same answer an
    /// unregistered server gets — so the tile draws its skeleton face and no prefetch budget is
    /// spent on it.
    ///
    /// It asserts only the BUILDER, and that limit is the host boundary rather than an oversight:
    /// calling `poster_get_wh`/`poster_warm` here — even on the empty key they return early for —
    /// makes `lookup` reachable from the test binary, and through it `gfx::delete_tex`. The link
    /// then fails with `Undefined symbols … _glDeleteTextures`, because `-dead_strip` was the only
    /// reason this suite got away without a GL context at all. Their empty-key early exits are
    /// asserted in prose at their definitions instead.
    #[test]
    fn an_empty_thumb_yields_no_request_at_all() {
        let (_g, sid, _) = one_server();
        assert_eq!(
            key_for(sid, "", 300, 300, 0),
            "",
            "an empty source must not become a request"
        );
    }




    /// The cliff absolute URLs brought within sight, and the reason [`poster_key`] gates on
    /// [`KEY_MAX`] instead of trusting the caller's 352-byte buffer.
    ///
    /// `set_key` truncates into [`PT_KEYLEN`]. A truncated key never equals the probe that built
    /// it, so the slot can never be found again: every frame misses, claims a fresh slot and
    /// evicts a real poster — the whole store thrashing over one tile, which is far worse than the
    /// tile simply not loading. The sizes are DERIVED from a probe build rather than written down,
    /// so this keeps grading the real boundary if the request shape ever changes.
    #[test]
    fn a_path_too_long_for_a_slot_is_refused_rather_than_truncated() {
        let (_g, sid, _) = one_server();
        // Everything around the source, measured: build with a one-character source and subtract
        // it. The filler is alphanumeric, so it passes through `enc` one byte per byte.
        let overhead = key_for(sid, "x", 300, 300, 0).len() - 1;

        let exact = key_for(sid, &"a".repeat(KEY_MAX - overhead), 300, 300, 0);
        assert_eq!(
            exact.len(),
            KEY_MAX,
            "the fixture must land exactly on the boundary"
        );
        let mut s = Pslot::ZERO;
        set_key(&mut s, &exact);
        assert_eq!(
            key_bytes(&s),
            exact.as_bytes(),
            "a key the gate accepts must survive a slot intact"
        );

        let over = key_for(sid, &"a".repeat(KEY_MAX - overhead + 1), 300, 300, 0);
        assert_eq!(
            over, "",
            "one byte past what a slot holds must be refused, not handed out"
        );

        // …because this is what handing it out would have meant. Not a claim about `set_key`, a
        // claim about `lookup`: the probe it compares against is the FULL string.
        let would_be = "b".repeat(KEY_MAX + 1);
        set_key(&mut s, &would_be);
        assert_ne!(
            key_bytes(&s),
            would_be.as_bytes(),
            "a truncated slot can never match its own probe"
        );
    }

    /// The store's precondition, asserted where it actually lives.
    ///
    /// `poster_key` refusing is only half the guarantee: [`lookup`] takes a `&str` from three
    /// `pub(crate)` entry points that each accept a raw `*const c_char`, so a key built by any
    /// other route reaches the 256-byte array without passing the producer's gate. Testing
    /// [`is_fetchable`] directly is the whole reason it was split out — `lookup` itself cannot be
    /// called from a host test binary (it reaches `gfx::delete_tex`, and nothing here links GL).
    #[test]
    fn only_a_key_that_survives_a_slot_is_fetchable() {
        assert!(!is_fetchable(""), "an empty key names no request");
        assert!(
            is_fetchable("/photo/:/transcode?url=x"),
            "an ordinary key is fetchable"
        );
        assert!(
            is_fetchable(&"a".repeat(KEY_MAX)),
            "exactly what a slot holds is still fetchable"
        );
        assert!(
            !is_fetchable(&"a".repeat(KEY_MAX + 1)),
            "one byte past the array must never claim a slot"
        );
    }

    /// A settled, drawable slot: READY, last touched on frame `frame` with LRU age `use_`.
    fn ready(use_: c_uint, frame: c_uint) -> Pslot {
        Pslot {
            state: P_READY,
            use_,
            frame,
            ..Pslot::ZERO
        }
    }

    /// A queued request no worker has picked up: P_WANT, last drawn on frame `frame`, LRU age `use_`.
    fn want(frame: c_uint, use_: c_uint, visible: bool) -> Pslot {
        Pslot { state: P_WANT, visible, frame, use_, ..Pslot::ZERO }
    }





    /// A rejected decode used to leave the source in READY while the cache remembered the key as
    /// failed. No later probe could re-arm either half, so the tile drew its skeleton forever.
    #[test]
    fn a_rejected_decode_cannot_leave_the_real_source_claiming_ready() {
        let (_fresh, sid, _) = one_server();
        tex::install(&SOURCE);
        tex::reset_for_test(16);
        let path = key_for(sid, "/library/metadata/42/thumb", 2, 2, 0);
        {
            let mut g = store();
            g.slots = [Pslot::ZERO; PT_CAP];
            let slot = &mut g.slots[0];
            slot.srv = sid;
            slot.cache_gen = plx_platform::imgcache::generation();
            set_key(slot, &path);
            slot.state = P_DECODED;
            slot.px = 0;
            slot.pw = 0;
            slot.ph = 0;
        }

        drain_decoded();

        assert_eq!(
            store().slots[0].state,
            P_EVICTED,
            "a cache rejection must synchronously revoke the source's READY claim"
        );
    }

    /// The whole product loop, using the installed [`PosterSource`], the thread-local product
    /// cache and the global poster store: resident → byte-pressure release → dormant EVICTED → a
    /// real DRAW probe → WANT + condition-variable wake → worker publication → resident again. A
    /// warm probe would leave the slot dormant (see
    /// [`a_warm_probe_of_an_evicted_slot_leaves_it_dormant`]) — this exercises the recovery path
    /// that still works, a real draw.
    #[test]
    fn a_ready_source_hit_recovers_after_its_texture_is_evicted() {
        struct StubUp {
            next: u32,
        }
        impl Uploader for StubUp {
            fn upload(&mut self, d: &Decoded) -> Tex {
                self.next += 1;
                Tex {
                    id: self.next,
                    w: d.w,
                    h: d.h,
                }
            }
            fn warm(&mut self, _: Tex) {}
            fn free(&mut self, _: Tex) {}
        }

        const BYTES: usize = 16;
        const SRC: &str = "/library/metadata/42/thumb";
        let (_fresh, srv, _) = one_server();
        tex::install(&SOURCE);
        tex::reset_for_test(BYTES);
        let path = key_for(srv, SRC, 2, 2, 0);
        {
            let mut g = store();
            g.slots = [Pslot::ZERO; PT_CAP];
            g.quit = false;
            let slot = &mut g.slots[0];
            slot.state = P_READY;
            slot.srv = srv;
            slot.cache_gen = plx_platform::imgcache::generation();
            set_key(slot, &path);
        }

        let decoded = |key| PosterReady {
            key,
            result: Ok(Decoded {
                w: 2,
                h: 2,
                rgba: vec![0; BYTES].into_boxed_slice(),
            }),
        };
        tex::accept(decoded(PosterKey(0)));
        let mut budget = plx_ui::frame::Budget::new();
        budget.begin_frame(0);
        let mut present = plx_machine::present::Present::new();
        let mut present_handle = plx_machine::machine::PresentHandle::of(&mut present);
        let mut uploader = StubUp { next: 0 };
        assert_eq!(
            tex::prepare(&mut budget, &mut uploader, &mut present_handle, || 0),
            1
        );
        assert_ne!(tex::resolve_on(srv.raw(), SRC, 2, 2, false), 0);

        // One frame with nothing drawn: `TexCache::evict_for`'s on-screen protection (this key
        // stays resident through the frame immediately after it was drawn — the frame it would
        // still be on screen for, if it still were) has to lapse before pressure may take it.
        // Without this idle frame the very next `prepare` would refuse to evict a texture the
        // prior frame just drew, exactly the "arrivals lose" rule the fix added.
        budget.begin_frame(10_000);
        let mut present_handle = plx_machine::machine::PresentHandle::of(&mut present);
        assert_eq!(
            tex::prepare(&mut budget, &mut uploader, &mut present_handle, || 10_000),
            0,
            "nothing pending on the idle settle frame"
        );

        tex::accept(decoded(PosterKey(1)));
        budget.begin_frame(20_000);
        let mut present_handle = plx_machine::machine::PresentHandle::of(&mut present);
        assert_eq!(
            tex::prepare(&mut budget, &mut uploader, &mut present_handle, || 20_000),
            1,
            "the second upload must evict the first by bytes through the product wrapper, now \
             that it is no longer within the on-screen protection window"
        );
        assert_eq!(store().slots[0].state, P_EVICTED);

        let (armed_tx, armed_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let g = store();
            armed_tx.send(()).unwrap();
            let (mut g, timeout) = CV
                .wait_timeout_while(g, Duration::from_secs(1), |s| s.slots[0].state != P_WANT)
                .unwrap_or_else(|e| e.into_inner());
            assert!(!timeout.timed_out(), "the draw probe must wake a waiting poster worker");
            let slot = &mut g.slots[0];
            slot.state = P_LOADING;
            drop(g);

            unsafe extern "C" {
                fn malloc(size: usize) -> *mut std::ffi::c_void;
            }
            let px = unsafe { malloc(BYTES) as *mut c_uchar };
            assert!(!px.is_null());
            unsafe { std::ptr::write_bytes(px, 0, BYTES) };
            let mut g = store();
            let slot = &mut g.slots[0];
            slot.px = px as usize;
            slot.pw = 2;
            slot.ph = 2;
            slot.state = P_DECODED;
        });
        armed_rx.recv().unwrap();
        assert_eq!(
            tex::resolve_on(srv.raw(), SRC, 2, 2, false),
            0,
            "the re-armed slot has no texture yet - a draw re-arms the dormant source slot"
        );
        worker.join().unwrap();
        assert_eq!(store().slots[0].state, P_DECODED, "the woken worker published pixels");

        drain_decoded();
        budget.begin_frame(40_000);
        let mut present_handle = plx_machine::machine::PresentHandle::of(&mut present);
        assert_eq!(
            tex::prepare(&mut budget, &mut uploader, &mut present_handle, || 40_000),
            1
        );
        assert_ne!(
            tex::resolve_on(srv.raw(), SRC, 2, 2, false),
            0,
            "the same source key becomes resident again"
        );

        tex::shutdown(&mut uploader);
        store().slots = [Pslot::ZERO; PT_CAP];
    }

    /// The product path for a REFUSED arrival (as opposed to a genuine pressure release, which
    /// [`a_ready_source_hit_recovers_after_its_texture_is_evicted`] already covers): a brand-new
    /// key whose only eviction candidate was drawn a moment ago must never become resident, and
    /// the source side of that refusal must look right — `P_EVICTED` with no cooldown (a key's
    /// first-ever eviction never gets one, [`evict_was_rapid`]'s doc), re-armed by the very next
    /// Draw probe, and counted as a REFUSAL (`RESIDENCY_REFUSED`) rather than a loss
    /// (`RESIDENCY_LOST`) — the distinction Opus's review of the on-screen eviction-protection fix
    /// found missing: a rejected arrival's slot is `P_READY` exactly like a genuinely evicted one,
    /// so without this counter split the device log's `posters: residency lost=` line kept
    /// climbing on an over-budget idle screen even once no on-screen texture was actually being
    /// evicted any more. Watched red against the code before that split: `refused1` read `0` and
    /// `lost1` carried the transition instead.
    #[test]
    fn a_refused_arrival_is_not_counted_as_a_residency_loss() {
        struct StubUp {
            next: u32,
        }
        impl Uploader for StubUp {
            fn upload(&mut self, d: &Decoded) -> Tex {
                self.next += 1;
                Tex { id: self.next, w: d.w, h: d.h }
            }
            fn warm(&mut self, _: Tex) {}
            fn free(&mut self, _: Tex) {}
        }

        const BYTES: usize = 16; // the byte ceiling below fits exactly one of these
        const SRC0: &str = "/library/metadata/42/thumb";
        const SRC1: &str = "/library/metadata/43/thumb";
        let (_fresh, srv, _) = one_server();
        tex::install(&SOURCE);
        tex::reset_for_test(BYTES);
        let path0 = key_for(srv, SRC0, 2, 2, 0);
        let path1 = key_for(srv, SRC1, 2, 2, 0);
        {
            let mut g = store();
            g.slots = [Pslot::ZERO; PT_CAP];
            g.quit = false;
            let cache_gen = plx_platform::imgcache::generation();
            for (i, path) in [&path0, &path1].into_iter().enumerate() {
                let slot = &mut g.slots[i];
                slot.state = P_READY;
                slot.srv = srv;
                slot.cache_gen = cache_gen;
                set_key(slot, path);
            }
        }

        let decoded = |key| PosterReady {
            key,
            result: Ok(Decoded { w: 2, h: 2, rgba: vec![0; BYTES].into_boxed_slice() }),
        };
        let mut budget = plx_ui::frame::Budget::new();
        let mut present = plx_machine::present::Present::new();
        let mut uploader = StubUp { next: 0 };
        let (lost0, _) = residency_counts_for_test();
        let refused0 = residency_refused_for_test();

        // Key 0 arrives, uploads, and is drawn — resident AND on screen this frame.
        tex::accept(decoded(PosterKey(0)));
        budget.begin_frame(0);
        let mut present_handle = plx_machine::machine::PresentHandle::of(&mut present);
        assert_eq!(tex::prepare(&mut budget, &mut uploader, &mut present_handle, || 0), 1);
        assert_ne!(tex::resolve_on(srv.raw(), SRC0, 2, 2, false), 0, "key 0 is resident and drawn");

        // Key 1 arrives on the VERY NEXT frame, needing key 0's bytes to fit under the ceiling.
        // Key 0 was drawn last frame, so it is protected: key 1 must be refused, not admitted by
        // flipping key 0 off screen.
        tex::accept(decoded(PosterKey(1)));
        budget.begin_frame(10_000);
        let mut present_handle = plx_machine::machine::PresentHandle::of(&mut present);
        assert_eq!(
            tex::prepare(&mut budget, &mut uploader, &mut present_handle, || 10_000),
            0,
            "the arrival must be refused - its only eviction candidate was drawn last frame"
        );
        assert_ne!(
            tex::resolve_on(srv.raw(), SRC0, 2, 2, false),
            0,
            "key 0 must still be resident - it was never evicted"
        );

        assert_eq!(
            store().slots[1].state, P_EVICTED,
            "the refused arrival's slot is demoted exactly like a genuine eviction"
        );
        assert!(
            store().slots[1].evict_cooldown_until.is_none(),
            "a key's first-ever eviction (real or refused) never sets a cooldown"
        );
        let (lost1, _) = residency_counts_for_test();
        let refused1 = residency_refused_for_test();
        assert_eq!(lost1, lost0, "a refused arrival was never resident - it must not count as a loss");
        assert_eq!(refused1, refused0 + 1, "the refusal must count exactly once");

        // The next Draw probe re-arms the dormant slot, as any uncooled EVICTED slot would.
        let (hit, _) = lookup(srv, &path1, Touch::Draw);
        assert_eq!(hit, None, "the re-armed slot has no pixels yet");
        assert_eq!(store().slots[1].state, P_WANT, "the refusal is fully recoverable");

        tex::shutdown(&mut uploader);
        store().slots = [Pslot::ZERO; PT_CAP];
    }

    /// Under `pitv-cardspeed` (the guard), a DRAW in a fast-moving card scope claims no slot
    /// for a miss. The same draw claims one when that scope permits admission. Demand is
    /// deferred, never dropped: the next frame asks again without leaving a queued request behind.
    #[test]
    fn a_fast_scroll_declines_a_visible_miss_and_a_settle_takes_it() {
        let (_fresh, sid, _) = one_server();
        let path = key_for(sid, "/library/metadata/4242/thumb", 2, 2, 0);
        store().slots = [Pslot::ZERO; PT_CAP];

        let _enforce = plx_ui::card_motion::enforce_for_test();
        let motion = plx_ui::card_motion::Scope::moving_for_test();
        assert!(plx_ui::card_motion::declines_request(), "the fixture is a fast scroll");
        let (hit, warm) = lookup(sid, &path, Touch::Draw);
        assert_eq!(hit, None, "a fast-scrolling miss resolves to no poster");
        assert_eq!(warm, Warm::Full, "and reports the attempt spent");
        assert!(store().slots.iter().all(|s| s.state == P_EMPTY),
            "no slot is claimed, so no worker, fetch, decode or upload follows");

        // The settle is the whole difference — same key, same store, same call.
        drop(motion);
        assert!(!plx_ui::card_motion::declines_request());
        let (_, warm) = lookup(sid, &path, Touch::Draw);
        assert_eq!(warm, Warm::Claimed, "a settled document claims the slot");
        assert_eq!(store().slots[0].state, P_WANT, "and queues it for a worker");
        assert!(store().slots[0].visible, "as visible demand, not speculation");

        store().slots = [Pslot::ZERO; PT_CAP];
    }

    /// The shipping default: a fast-moving card is not refused. Only an `Unknown` placement is.
    #[test]
    fn by_default_a_fast_scroll_claims_the_slot_and_only_an_unknown_placement_declines() {
        let (_fresh, sid, _) = one_server();
        let _default = plx_ui::card_motion::default_for_test();
        let path = key_for(sid, "/library/metadata/4245/thumb", 2, 2, 0);
        store().slots = [Pslot::ZERO; PT_CAP];

        {
            let _unknown = plx_ui::card_motion::Scope::unknown_for_test();
            assert!(plx_ui::card_motion::declines_request());
            let (_, warm) = lookup(sid, &path, Touch::Draw);
            assert_eq!(warm, Warm::Full, "an unknown placement still waits for its second sample");
            assert!(store().slots.iter().all(|s| s.state == P_EMPTY));
        }
        let _motion = plx_ui::card_motion::Scope::moving_for_test();
        assert!(!plx_ui::card_motion::declines_request(), "the fixture is a fast scroll, and it is admitted");
        let (_, warm) = lookup(sid, &path, Touch::Draw);
        assert_eq!(warm, Warm::Claimed, "a moving card claims its slot at once");
        assert_eq!(store().slots[0].state, P_WANT);
        assert!(store().slots[0].visible);

        store().slots = [Pslot::ZERO; PT_CAP];
    }

    /// The wiring `declines_in` is a pure function of: in a frame dump the SAME fast-moving draw
    /// claims a slot, because a dump never declines for motion (`card_motion::declines_request`),
    /// even with `pitv-cardspeed` armed.
    #[test]
    fn an_armed_dump_claims_a_slot_for_the_miss_a_fast_scroll_would_decline() {
        let (_fresh, sid, _) = one_server();
        let path = key_for(sid, "/library/metadata/4243/thumb", 2, 2, 0);
        store().slots = [Pslot::ZERO; PT_CAP];

        let _enforce = plx_ui::card_motion::enforce_for_test();
        let _motion = plx_ui::card_motion::Scope::moving_for_test();
        assert!(plx_ui::card_motion::declines_request(), "the fixture is a fast scroll");
        {
            let _dump = plx_gfx::dump::Armed::new();
            assert!(!plx_ui::card_motion::declines_request(), "armed, motion declines nothing");
            let (_, warm) = lookup(sid, &path, Touch::Draw);
            assert_eq!(warm, Warm::Claimed, "the draw claims the slot a fast scroll would have declined");
            assert_eq!(store().slots[0].state, P_WANT);
        }
        assert!(plx_ui::card_motion::declines_request(), "disarmed, the same scope declines again");
        store().slots = [Pslot::ZERO; PT_CAP];
    }





    /// **P2 (Codex review on PR #187).** The fresh-miss gate above is the LEAST important of the
    /// three places a draw starts poster work, because a fast scroll is the very thing that
    /// evicts textures: scroll down hard, and the rows behind you lose residency; scroll back,
    /// and every tile you pass finds its own slot parked at `P_EVICTED` with no cooldown set
    /// (a first eviction never cools). Re-arming each one is a full fetch, decode and upload —
    /// the work the gate exists to refuse — reached through a branch that returns long before
    /// the miss path. The deferral is the cooldown's, not a drop: the slot stays `P_EVICTED`
    /// and the first settled draw re-arms it exactly as it always did.
    #[test]
    fn a_fast_scroll_declines_to_re_arm_an_evicted_slot() {
        let (_fresh, sid, _) = one_server();
        let path = key_for(sid, "/library/metadata/4243/thumb", 2, 2, 0);
        {
            let mut g = store();
            g.slots = [Pslot::ZERO; PT_CAP];
            let slot = &mut g.slots[0];
            slot.srv = sid;
            slot.cache_gen = plx_platform::imgcache::generation();
            set_key(slot, &path);
            slot.state = P_EVICTED;
            // No eviction history, so the cooldown gate would admit this re-arm. That isolates
            // the scroll gate: nothing else in this branch is refusing.
            slot.evict_cooldown_until = None;
        }
        let (_, rearmed0) = residency_counts_for_test();

        let _enforce = plx_ui::card_motion::enforce_for_test();
        let motion = plx_ui::card_motion::Scope::moving_for_test();
        let (hit, warm) = lookup(sid, &path, Touch::Draw);
        assert_eq!(hit, None, "an evicted slot has no pixels to hand back");
        assert_eq!(warm, Warm::Known, "the slot is known, merely left dormant");
        assert_eq!(
            store().slots[0].state,
            P_EVICTED,
            "a fast scroll must not re-arm an evicted slot: that is a whole fetch and upload"
        );
        let (_, rearmed1) = residency_counts_for_test();
        assert_eq!(rearmed1, rearmed0, "and must not count as a re-arm");

        drop(motion);
        let (_, _) = lookup(sid, &path, Touch::Draw);
        assert_eq!(store().slots[0].state, P_WANT, "a settled draw re-arms it exactly as before");
        let (_, rearmed2) = residency_counts_for_test();
        assert_eq!(rearmed2, rearmed0 + 1, "and the settle is what the counter records");

        store().slots = [Pslot::ZERO; PT_CAP];
    }


    /// The two counters behind `posters: residency lost=… rearmed=…` (issue #107): the route
    /// heartbeat's `evicted_hot=` cannot distinguish "eviction never fired" from "eviction fired
    /// and the re-arm worked", so these are graded directly rather than through the log line's
    /// throttle. `one_server` takes [`plx_base::testlock::serial`], which is what keeps this test's
    /// deltas exact against the other tests in this file that drive real eviction through the
    /// installed cache (`a_rejected_decode_cannot_leave_the_real_source_claiming_ready`,
    /// `a_ready_source_hit_recovers_after_its_texture_is_evicted`).
    #[test]
    fn residency_transitions_are_counted_exactly_once_each() {
        use plx_ui::tex::Source as _;

        let (_fresh, sid, _) = one_server();
        let path = key_for(sid, "/library/metadata/42/thumb", 2, 2, 0);
        {
            let mut g = store();
            g.slots = [Pslot::ZERO; PT_CAP];
            let slot = &mut g.slots[0];
            slot.srv = sid;
            slot.cache_gen = plx_platform::imgcache::generation();
            set_key(slot, &path);
            slot.state = P_READY;
        }
        let (lost0, rearmed0) = residency_counts_for_test();

        SOURCE.unresident(PosterKey(0), false);
        assert_eq!(store().slots[0].state, P_EVICTED, "unresident demotes a READY slot");
        let (lost1, rearmed1) = residency_counts_for_test();
        assert_eq!(lost1, lost0 + 1, "a real READY->EVICTED transition must count exactly once");
        assert_eq!(rearmed1, rearmed0, "unresident never touches the re-arm counter");

        // A slot already EVICTED has no READY truth left to revoke - must not count again.
        SOURCE.unresident(PosterKey(0), false);
        let (lost2, _) = residency_counts_for_test();
        assert_eq!(lost2, lost1, "a slot that was already EVICTED must not be counted twice");

        let (hit, _) = lookup(sid, &path, Touch::Draw);
        assert_eq!(hit, None, "a freshly re-armed slot has no pixels yet");
        assert_eq!(store().slots[0].state, P_WANT, "lookup re-arms the dormant slot");
        let (_, rearmed2) = residency_counts_for_test();
        assert_eq!(rearmed2, rearmed1 + 1, "the EVICTED->WANT re-arm must count exactly once");

        store().slots = [Pslot::ZERO; PT_CAP];
    }

    /// **P1 (Codex review on PR #182, `lookup`'s P_EVICTED branch, poster.rs:564).** Without the
    /// thrash guard, every draw that finds a key EVICTED moves it straight back to `P_WANT`
    /// unconditionally, with no memory of how recently it was evicted before. A working set that
    /// genuinely cannot fit under the render cache's byte budget then has this key evicted,
    /// re-armed, evicted again, re-armed again — forever, once frames keep drawing it — instead of
    /// settling. This drives exactly that: a slot evicted and (synthetically, via the same
    /// `SOURCE.unresident` callback the render cache calls under real byte pressure) re-evicted in
    /// rapid succession, every cycle immediately followed by a draw. Pre-fix, EVERY cycle rearms —
    /// no bound at all. Post-fix, only the first (uncooled) eviction rearms; the rest are refused
    /// until their cooldown clears, which real time never does inside a tight loop.
    #[test]
    fn a_key_evicted_and_reevicted_in_rapid_succession_does_not_rearm_without_bound() {
        use plx_ui::tex::Source as _;

        let (_fresh, sid, _) = one_server();
        let path = key_for(sid, "/library/metadata/42/thumb", 2, 2, 0);
        {
            let mut g = store();
            g.slots = [Pslot::ZERO; PT_CAP];
            let slot = &mut g.slots[0];
            slot.srv = sid;
            slot.cache_gen = plx_platform::imgcache::generation();
            set_key(slot, &path);
            slot.state = P_READY;
        }
        let (_, rearmed0) = residency_counts_for_test();

        const CYCLES: u32 = 20;
        let mut rearms = 0u32;
        for _ in 0..CYCLES {
            // Stand-in for the render cache's own byte-pressure eviction (real pressure needs a
            // live GL cache no host test links; this callback IS the seam it calls through).
            SOURCE.unresident(PosterKey(0), false);
            let (hit, _) = lookup(sid, &path, Touch::Draw);
            assert_eq!(hit, None, "an evicted slot never has pixels the same probe that finds it");
            if store().slots[0].state == P_WANT {
                rearms += 1;
                // Simulate the fetch/decode/upload this rearm triggers completing immediately —
                // the fast round trip a thrash needs, not the slow one a network stall would add.
                store().slots[0].state = P_READY;
            }
        }
        let (_, rearmed_final) = residency_counts_for_test();
        assert_eq!(
            rearmed_final - rearmed0,
            rearms as u64,
            "the rearm counter must track exactly the transitions this loop drove"
        );

        assert!(
            rearms < CYCLES,
            "a key evicted and re-evicted in rapid succession must eventually back off instead \
             of rearming every single cycle (got {rearms} rearms out of {CYCLES} cycles - with \
             no bound this is the P1 thrash: continuous fetch/decode/upload instead of settling)"
        );
        assert_eq!(
            rearms, 1,
            "the FIRST eviction must still rearm instantly (the bug PR #182 itself fixes); every \
             eviction after it lands inside the thrash window and must be cooled down"
        );

        store().slots = [Pslot::ZERO; PT_CAP];
    }

    /// The pure decisions behind the residency thrash guard (see `evict_was_rapid`'s module doc):
    /// a key's first-ever eviction is never rapid (nothing to compare against), one that lands
    /// inside the thrash window is, and one that lands outside it is treated as fresh again - the
    /// guard must not let a key that settles for a while accumulate backoff it no longer deserves.
    #[test]
    fn evict_was_rapid_only_flags_a_reeviction_inside_the_thrash_window() {
        assert!(!evict_was_rapid(None, 0), "a key's first-ever eviction has nothing to compare");
        assert!(
            evict_was_rapid(Some(0), EVICT_THRASH_WINDOW_MS - 1),
            "one millisecond inside the window is still rapid"
        );
        assert!(
            !evict_was_rapid(Some(0), EVICT_THRASH_WINDOW_MS),
            "the window's own edge is no longer rapid"
        );
    }

    /// The pure decision behind the thrash guard's cooldown gate: no cooldown ever set is always
    /// due (the ordinary, non-thrashing case), and a set cooldown is due only once its deadline has
    /// actually passed (wrap-safe).
    ///
    /// Through the pure twin with the dump switch passed as `false`: this test holds no
    /// `testlock::serial()`, and `an_armed_dump_claims_a_slot_...` arms the process-global switch in
    /// the same binary, so the global-reading `evict_cooldown_due` could answer `true` here for the
    /// few statements the switch is up.
    #[test]
    fn evict_cooldown_due_only_after_its_own_deadline() {
        assert!(evict_cooldown_due_in(false, None, 0), "no cooldown was ever set - never gated");
        assert!(!evict_cooldown_due_in(false, Some(1_000), 999), "one millisecond short is still cooling");
        assert!(evict_cooldown_due_in(false, Some(1_000), 1_000), "the deadline itself has cleared");
    }

    #[test]
    fn a_frame_dump_has_no_eviction_cooldown_and_names_the_first_failed_request() {
        assert!(!evict_cooldown_due_in(false, Some(1_000), 999));
        assert!(evict_cooldown_due_in(true, Some(1_000), 999), "a frozen clock would never clear it");
        let mut g = DUMP_FAILURE.lock().unwrap_or_else(|e| e.into_inner());
        *g = None;
        drop(g);
        note_dump_failure(b"local:200x300:poster-9", false);
        note_dump_failure(b"local:200x300:second", true);
        let g = DUMP_FAILURE.lock().unwrap_or_else(|e| e.into_inner());
        let why = g.as_deref().expect("the first failure is kept");
        assert!(why.contains("final") && why.contains("poster-9"), "{why}");
        assert!(!why.contains("second"), "only the first is kept");
    }

    /// A second half of the same PR #182 P1: [`evict_cooldown_due_only_after_its_own_deadline`]
    /// and [`a_key_evicted_and_reevicted_in_rapid_succession_does_not_rearm_without_bound`] bound
    /// how OFTEN a DRAW may re-arm an evicted key; this proves a `Touch::Warm` probe of the same
    /// key may never re-arm it AT ALL, regardless of cooldown state — resurrecting an evicted key
    /// speculatively is exactly what let an off-screen prefetch alternate evict/refetch/evict
    /// against on-screen demand (Home's recurring backdrop prefetch, named in the review). The
    /// slot here has no eviction history (`evict_cooldown_until` is `None`, i.e. never cooling), so
    /// this isolates the touch-type gate from the cooldown gate: a warm probe is refused even when
    /// nothing else would have refused it. The second half proves the fix is a narrowing, not a
    /// removal: a real draw of the same slot still re-arms.
    #[test]
    fn a_warm_probe_of_an_evicted_slot_leaves_it_dormant() {
        let (_fresh, sid, _) = one_server();
        let path = key_for(sid, "/library/metadata/42/thumb", 2, 2, 0);
        {
            let mut g = store();
            g.slots = [Pslot::ZERO; PT_CAP];
            let slot = &mut g.slots[0];
            slot.srv = sid;
            slot.cache_gen = plx_platform::imgcache::generation();
            set_key(slot, &path);
            slot.state = P_EVICTED;
        }
        let (_, rearmed0) = residency_counts_for_test();

        let (hit, warm) = lookup(sid, &path, Touch::Warm);
        assert_eq!(hit, None, "an evicted slot has no pixels to hand back");
        assert_eq!(warm, Warm::Known, "a warm probe must not claim a slot it leaves dormant");
        assert_eq!(
            store().slots[0].state,
            P_EVICTED,
            "a warm probe must not re-arm an evicted slot"
        );
        let (_, rearmed1) = residency_counts_for_test();
        assert_eq!(rearmed1, rearmed0, "a warm probe must not increment the re-arm counter");

        // The same slot, probed by a real draw, still re-arms - the fix narrows WHICH touch
        // re-arms an evicted slot, it does not remove re-arming itself.
        let (hit, _) = lookup(sid, &path, Touch::Draw);
        assert_eq!(hit, None, "a freshly re-armed slot has no pixels yet");
        assert_eq!(store().slots[0].state, P_WANT, "a draw probe still re-arms the dormant slot");
        let (_, rearmed2) = residency_counts_for_test();
        assert_eq!(
            rearmed2,
            rearmed0 + 1,
            "the draw's EVICTED->WANT re-arm must count exactly once"
        );

        store().slots = [Pslot::ZERO; PT_CAP];
    }





    /// The pure decision behind [`invalidate_due_evictions`]: only a slot an active cooldown
    /// deadline is ticking against is ever due for a wake - a slot with no cooldown (`None`) has
    /// nothing to expire and needs none, since its very next Draw probe already re-arms it: a
    /// proactive wake for it would just be [`invalidate_due_retries`]'s own P_RETRY continuous-
    /// invalidation bug, reintroduced for P_EVICTED. Mirrors [`a_retry_slot_is_due_when_its_wait_is_over`]'s
    /// shape, including the wrap.
    #[test]
    fn an_evicted_slot_is_due_for_a_wake_only_once_its_cooldown_deadline_arrives() {
        let mut s = Pslot {
            state: P_EVICTED,
            ..Pslot::ZERO
        };
        assert!(!evict_wake_due(&s, 1_000), "no cooldown at all = nothing to wake for");
        s.evict_cooldown_until = Some(6_000);
        assert!(!evict_wake_due(&s, 1_000));
        assert!(evict_wake_due(&s, 6_000));
        assert!(evict_wake_due(&s, 9_000));
        s.evict_cooldown_until = Some(u32::MAX - 100);
        assert!(evict_wake_due(&s, 50), "the tick wraps like every app::clock comparison");
        s.state = P_READY;
        assert!(!evict_wake_due(&s, 60_000), "only an EVICTED slot can be due for this wake");
    }

    /// **Requirement 1's wiring, end to end: exactly one redraw request per expiry, not one per
    /// frame.** Mirrors [`a_due_parked_retry_invalidates_the_frame_gate`] exactly, because
    /// [`invalidate_due_evictions`] is the identical mechanism applied to `P_EVICTED` instead of
    /// `P_RETRY`, for the identical reason: without the `evict_wake_sent` latch, an expired,
    /// off-screen EVICTED slot would invalidate every loop iteration forever, since nothing draws
    /// it to consume the wake — undoing the idle behaviour the app depends on.
    #[test]
    fn a_due_evicted_slot_invalidates_the_frame_gate_exactly_once() {
        let _g = plx_base::testlock::serial();
        plx_machine::idle::reset_for_test();
        let mut slots = [Pslot::ZERO; PT_CAP];
        slots[3] = Pslot {
            state: P_EVICTED,
            evict_cooldown_until: Some(2_000),
            ..Pslot::ZERO
        };
        slots[4] = Pslot {
            state: P_EVICTED,
            evict_cooldown_until: Some(2_000),
            ..Pslot::ZERO
        };

        invalidate_due_evictions(&mut slots, 1_999);
        assert_eq!(
            plx_machine::idle::take_local_damage(),
            0,
            "a cooling slot must leave the screen settled before its deadline"
        );

        invalidate_due_evictions(&mut slots, 2_000);
        assert_eq!(
            plx_machine::idle::take_local_damage(),
            1,
            "the deadline must wake exactly one draw that can probe the slot again"
        );

        invalidate_due_evictions(&mut slots, 2_001);
        assert_eq!(
            plx_machine::idle::take_local_damage(),
            0,
            "an off-screen expired slot must not hold the present gate awake - the latch makes \
             this ONE redraw request, not a request every frame"
        );
    }

    /// The pure decision behind [`log_residency`]'s interval throttle: due before any line has
    /// ever been written, not due an instant after one was, due again once a full second has
    /// passed. Split out and tested the same way [`idle_of`] is, so the edges do not depend on a
    /// real elapsed second or a real Mutex.
    #[test]
    fn the_interval_throttle_only_waits_out_its_own_window() {
        let t0 = std::time::Instant::now();
        assert!(interval_due(None, t0), "the first line in a process is never throttled");
        assert!(
            !interval_due(Some(t0), t0),
            "immediately after a line, the same instant must not be due again"
        );
        assert!(
            !interval_due(Some(t0), t0 + Duration::from_millis(999)),
            "one millisecond short of the window must still be throttled"
        );
        assert!(
            interval_due(Some(t0), t0 + Duration::from_secs(1)),
            "a full second later the window has reopened"
        );
    }

    /// Reproduces the exact field defect this fix closes (Codex review on PR #182): a device
    /// session that read `lost=1 rearmed=0`, `lost=5 rearmed=0`, `lost=13 rearmed=0` and nothing
    /// else was reported as "the re-arm branch never fires" — but every one of those lines was
    /// written by an EVICTION, and [`log_residency`]'s interval throttle can swallow a re-arm
    /// that lands inside the same one-second window as the eviction that preceded it, which is
    /// exactly the ordinary one-eviction-then-recovery case. This drives that sequence directly:
    /// an eviction (unthrottled, since it is the first line) immediately followed by its re-arm
    /// (throttled, since under a second has passed) must leave `rearmed` stale in the last
    /// emitted line — proving the gap is real — and then a settled frame
    /// ([`log_residency_settled`], as [`begin_frame`] calls it every frame regardless of screen)
    /// must catch the totals up, because that path does not consult the interval at all.
    #[test]
    fn a_rearm_inside_the_throttle_window_still_reaches_the_log_once_the_store_settles() {
        use plx_ui::tex::Source as _;

        let (_fresh, sid, _) = one_server();
        reset_residency_log_for_test();
        let path = key_for(sid, "/library/metadata/42/thumb", 2, 2, 0);
        {
            let mut g = store();
            g.slots = [Pslot::ZERO; PT_CAP];
            let slot = &mut g.slots[0];
            slot.srv = sid;
            slot.cache_gen = plx_platform::imgcache::generation();
            set_key(slot, &path);
            slot.state = P_READY;
        }

        // The eviction: the first line in the (just reset) throttle window, so it writes.
        SOURCE.unresident(PosterKey(0), false);
        let (lost1, rearmed1) = residency_counts_for_test();
        assert_eq!(
            residency_last_emitted_for_test(),
            (lost1, rearmed1),
            "the unthrottled first line must report the eviction it just recorded"
        );

        // The re-arm, immediately after: real time between these two calls is microseconds, so
        // the interval throttle finds the window still open and suppresses the line.
        let (hit, _) = lookup(sid, &path, Touch::Draw);
        assert_eq!(hit, None, "a freshly re-armed slot has no pixels yet");
        let (lost2, rearmed2) = residency_counts_for_test();
        assert_eq!(rearmed2, rearmed1 + 1, "the re-arm itself is never missed - only the LINE is");
        assert_eq!(
            residency_last_emitted_for_test(),
            (lost1, rearmed1),
            "the throttled re-arm must leave the last WRITTEN line stale - this is the bug"
        );

        // The store is quiet (P_WANT, not fetching or decoding) - a settled frame must catch the
        // line up regardless of the interval, because the totals moved since the last line.
        log_residency_settled(true);
        assert_eq!(
            residency_last_emitted_for_test(),
            (lost2, rearmed2),
            "a settle snapshot must report the re-arm even though the interval throttle just \
             suppressed it"
        );

        store().slots = [Pslot::ZERO; PT_CAP];
    }

    /// A settle snapshot must never write a line when nothing changed - `log_residency_settled`
    /// runs every frame the store is quiet, and most quiet frames follow another quiet frame.
    #[test]
    fn a_settled_frame_with_nothing_new_writes_no_second_line() {
        let _g = plx_base::testlock::serial();
        reset_residency_log_for_test();
        let (lost, rearmed) = residency_counts_for_test();
        {
            let mut st = RESIDENCY_LOG.lock().unwrap_or_else(|e| e.into_inner());
            st.at = Some(std::time::Instant::now());
            st.lost = lost;
            st.rearmed = rearmed;
        }
        // Nothing transitioned since - the recorded pair already matches the live totals, so this
        // must be a no-op (observable only through the pair staying put; a real double-write
        // would be indistinguishable here, but `log_residency_settled`'s own equality check is
        // what `residency_last_emitted_for_test` is pinning).
        log_residency_settled(true);
        assert_eq!(residency_last_emitted_for_test(), (lost, rearmed));
    }







    /// **The test the whole prefetch rests on.** `Touch::Warm` writes `use_ = 0` and a frame stamp
    /// of `frame - 1`, so a warmed slot is simultaneously the oldest thing in the store and
    /// unprotected — it must lose to every slot this frame drew. And when the frame really has
    /// touched everything, the answer must be `None` ("all visible: skip"), never a slot in use.
    #[test]
    fn a_warmed_slot_is_evicted_before_anything_the_frame_drew() {
        let f: c_uint = 42;
        let mut slots = [ready(c_uint::MAX - 1, f); PT_CAP]; // 64 tiles this frame drew
        slots[31] = ready(0, f.wrapping_sub(1)); // …and one warmed a moment ago
        assert_eq!(
            victim(&slots, f),
            Some(31),
            "the prefetched slot is the cheapest to throw away"
        );

        let drawn = [ready(5, f); PT_CAP];
        assert_eq!(
            victim(&drawn, f),
            None,
            "a frame that drew every slot claims nothing"
        );

        // frame counters wrap (`wrapping_add` in poster_pump), and a warm at frame 0 stamps
        // c_uint::MAX — which must still read as "not this frame" rather than as protection.
        let mut wrapped = [ready(c_uint::MAX - 1, 0); PT_CAP];
        wrapped[2] = ready(0, 0u32.wrapping_sub(1));
        assert_eq!(victim(&wrapped, 0), Some(2));
    }

    /// The sharp edge behind the depth budget: a slot that is fetching is never a victim, so enough
    /// in-flight warms make a miss return `None` — a poster the user is looking at that is never
    /// even REQUESTED, which is worse than one that arrives late. Pinned in code, not just in prose.
    #[test]
    fn an_in_flight_slot_is_never_evicted() {
        for st in [P_WANT, P_LOADING, P_DECODED] {
            let slots = [Pslot {
                state: st,
                use_: 0,
                frame: 0,
                ..Pslot::ZERO
            }; PT_CAP];
            assert_eq!(victim(&slots, 7), None, "state {st} must not be evictable");
        }
    }



    /// The prefetch gate sees every stage of a fetch. If this ever loosens, the prefetch quietly
    /// starts competing with the tiles on screen for the two workers.
    #[test]
    fn idle_of_sees_every_stage_of_a_fetch() {
        assert!(
            idle_of(&[Pslot::ZERO; PT_CAP]),
            "an untouched store is idle"
        );
        for st in [P_READY, P_EVICTED, P_FAILED] {
            let slots = [Pslot {
                state: st,
                ..Pslot::ZERO
            }; PT_CAP];
            assert!(
                idle_of(&slots),
                "settled state {st} is not work in progress"
            );
        }
        for st in [P_WANT, P_LOADING, P_DECODED] {
            let mut slots = [Pslot {
                state: P_READY,
                ..Pslot::ZERO
            }; PT_CAP];
            slots[63] = Pslot {
                state: st,
                ..Pslot::ZERO
            };
            assert!(
                !idle_of(&slots),
                "one slot in state {st} is enough to hold the gate shut"
            );
        }
    }

    #[test]
    fn a_visible_claim_preempts_an_older_prefetch_backlog() {
        let mut slots = [Pslot::ZERO; PT_CAP];
        for slot in slots.iter_mut().take(12) {
            slot.state = P_WANT;
            slot.visible = false;
        }
        slots[47].state = P_WANT;
        slots[47].visible = true;

        assert_eq!(next_wanted(&slots), Some(47),
            "a tile being drawn must outrank every queued lookahead claim");
    }

    #[test]
    fn prefetch_admission_leaves_one_worker_for_visible_work() {
        let mut slots = [Pslot::ZERO; PT_CAP];
        assert!(warm_admissible(&slots), "an idle queue may look ahead");

        slots[9].state = P_LOADING;
        slots[9].visible = false;
        assert!(!warm_admissible(&slots),
            "one speculative fetch spends the entire prefetch allowance");

        slots[9] = Pslot::ZERO;
        slots[41].state = P_WANT;
        slots[41].visible = true;
        assert!(!warm_admissible(&slots),
            "visible work closes prefetch admission regardless of slot index");
    }

    // ---- A held scroll on a slow server -------------------------------------------------------

    /// **The slow-server scroll.** No worker ever runs here (the fetch outlasts the whole scroll),
    /// the grid is 6 columns by 5 rows and a new row arrives every 8 frames: by the time 64 slots
    /// are spoken for, most of them name rows that left the screen. The row on screen NOW must
    /// still find its slots, and the worker's next pick must be on screen, not the oldest request.
    #[test]
    fn a_held_scroll_on_a_slow_server_never_starves_the_rows_on_screen() {
        let (_fresh, sid, _) = one_server();
        {
            let mut g = store();
            g.slots = [Pslot::ZERO; PT_CAP];
            g.frame = 1000;
        }
        let key = |n: usize| key_for(sid, &format!("/library/metadata/{}/thumb", 9000 + n), 2, 2, 0);
        let (cols, rows) = (6usize, 5usize);
        let mut first_row = 0usize;
        for frame in 0..400usize {
            store().frame += 1;
            if frame % 8 == 0 {
                first_row += 1; // one new row every 8 frames: 6 cards / 8 frames at 60 fps = 45 cards/s
            }
            for c in 0..cols * rows {
                lookup(sid, &key(first_row * cols + c), Touch::Draw);
            }
            // After the first screenful the window is always fully queued: none of its cards was
            // refused for want of a slot.
            if frame >= 8 {
                let g = store();
                for c in 0..cols * rows {
                    let k = key(first_row * cols + c);
                    assert!(
                        g.slots.iter().any(|s| s.state == P_WANT && s.visible && key_bytes(s) == k.as_bytes()),
                        "frame {frame}: on-screen card {c} has no queued request (store full of dead requests)"
                    );
                }
            }
        }
        // The worker's next pick is a card on screen, not the oldest request in slot order.
        let g = store();
        let picked = next_wanted(&g.slots).expect("work is queued");
        let on_screen: Vec<String> = (0..cols * rows).map(|c| key(first_row * cols + c)).collect();
        assert!(
            on_screen.iter().any(|k| k.as_bytes() == key_bytes(&g.slots[picked])),
            "the worker served a row that already left the screen"
        );
        drop(g);
        store().slots = [Pslot::ZERO; PT_CAP];
    }

    /// **A poster that is showing is never the victim.** The shape every held walk on a slow
    /// server reaches (`tests/paging_link.py`, imgtrace `HIDDEN gap=1f cause=recycled`): the
    /// requests of rows long gone fill the store, the only settled slots left are the cards on
    /// screen, and the frame's first miss runs before the draws that would stamp them. A request
    /// nobody draws goes first; with none, the miss waits ("store full") rather than blank a card.
    #[test]
    fn a_miss_recycles_a_dead_request_before_a_poster_drawn_last_frame() {
        let f: c_uint = 100;
        let drawn = |use_, frame| Pslot { visible: true, ..ready(use_, frame) };
        let mut slots: [Pslot; PT_CAP] =
            std::array::from_fn(|i| want(f - 40, 100 + i as c_uint, true)); // rows that scrolled away
        for (i, s) in slots.iter_mut().enumerate().take(30) {
            *s = drawn(1000 + i as c_uint, f - 1); // the screenful the last frame drew
        }
        let pick = victim(&slots, f).expect("a dead request is there to recycle");
        assert_eq!(slots[pick].state, P_WANT, "the miss recycled a poster that was on screen (slot {pick})");
        assert_eq!(pick, 30, "the request drawn longest ago goes");
        // Nothing but the screen and the workers' own slots: the newcomer waits a frame.
        for s in slots.iter_mut().skip(30) {
            s.state = P_LOADING;
        }
        assert_eq!(victim(&slots, f), None, "no picture on screen is given up for a new card");
        // A card that left the screen is ordinary LRU again once the window has passed.
        slots[7].frame = f - WANT_STALE_FRAMES;
        assert_eq!(victim(&slots, f), Some(7));
        // The lookahead's claim obeys the same rule (it had only recency before).
        assert_eq!(ahead_victim(&[drawn(5, f - 1); PT_CAP], f), None);
    }

    /// The same defect through the store itself: a held scroll whose server answers slower than
    /// the rows pass, walking BACK (up a grid, left along a shelf), so the arriving row is the first
    /// drawn and its misses run before the rest of the screen is stamped. Whatever arrived and is
    /// still on screen stays READY on every later frame.
    #[test]
    fn a_held_scroll_on_a_slow_server_keeps_every_poster_that_is_showing() {
        let (_fresh, sid, _) = one_server();
        {
            let mut g = store();
            g.slots = [Pslot::ZERO; PT_CAP];
            g.frame = 1000;
        }
        let key = |n: usize| key_for(sid, &format!("/library/metadata/{}/thumb", 9000 + n), 2, 2, 0);
        let (cols, rows) = (6usize, 5usize);
        let mut first_row = 60usize;
        let mut showing: Vec<usize> = Vec::new(); // card numbers READY when the last frame ended
        let (mut arrived, mut lost) = (0usize, Vec::new());
        for frame in 0..400usize {
            store().frame += 1;
            if frame % 8 == 0 {
                first_row -= 1;
            }
            let on_screen = first_row * cols..(first_row + rows) * cols;
            for n in on_screen.clone() {
                lookup(sid, &key(n), Touch::Draw);
            }
            let mut g = store();
            let is_ready = |g: &Store, n: usize| {
                let k = key(n);
                g.slots.iter().any(|s| s.state == P_READY && key_bytes(s) == k.as_bytes())
            };
            for &n in showing.iter().filter(|&&n| on_screen.contains(&n)) {
                if !is_ready(&g, n) {
                    lost.push((frame, n));
                }
            }
            // The slow server: one picture lands every 4 frames, the worker's own choice of which.
            if frame % 4 == 0 {
                if let Some(i) = next_wanted(&g.slots) {
                    g.slots[i].state = P_READY;
                    arrived += 1;
                }
            }
            showing = on_screen.clone().filter(|&n| is_ready(&g, n)).collect();
        }
        store().slots = [Pslot::ZERO; PT_CAP];
        assert!(arrived > 50, "the scenario delivered {arrived} pictures");
        assert!(lost.is_empty(), "{} posters that were showing lost their slot, first (frame, card) {:?}",
            lost.len(), lost.first());
    }

    /// The store's rule for a stale request: last resort only, drawn demand only, never in flight.
    #[test]
    fn a_stale_queued_request_is_recycled_only_when_nothing_better_exists() {
        let f: c_uint = 100;
        let stale = f - WANT_STALE_FRAMES;
        // Every slot a request: the stale one with the oldest draw goes; a fresh one never does.
        let mut slots: [Pslot; PT_CAP] = std::array::from_fn(|i| want(f - 1, 1000 + i as c_uint, true));
        slots[5] = want(stale, 400, true);
        slots[9] = want(stale - 3, 300, true);
        assert_eq!(victim(&slots, f), Some(9), "the request drawn longest ago is the one a draw recycles");
        slots[9].state = P_LOADING;
        assert_eq!(victim(&slots, f), Some(5), "a request a worker holds is never recycled");
        slots[5].frame = stale + 1;
        assert_eq!(victim(&slots, f), None, "a request drawn within the window is a card still on screen");
        // Speculation (a warm, a lookahead entry) was never drawn: not this clause's to take.
        slots[5] = want(0, 1, false);
        assert_eq!(victim(&slots, f), None);
        slots[5] = Pslot { ahead: true, ..want(0, 1, false) };
        assert_eq!(victim(&slots, f), None);
        // A settled slot is always preferred to a stale request: no picture is thrown away for it.
        slots[5] = want(stale, 400, true);
        slots[20] = ready(2000, f - 1);
        assert_eq!(victim(&slots, f), Some(20));
        // An empty slot first of all.
        slots[33] = Pslot::ZERO;
        assert_eq!(victim(&slots, f), Some(33));
    }

    /// The lookahead's victim is untouched: a stale drawn request is not lookahead's to take.
    #[test]
    fn a_lookahead_claim_never_recycles_a_queued_request() {
        let f: c_uint = 100;
        let slots: [Pslot; PT_CAP] = std::array::from_fn(|i| want(1, 1 + i as c_uint, true));
        assert_eq!(ahead_victim(&slots, f), None);
    }

    /// Queued DRAWN requests are served newest-draw first (ties in reading order); speculation
    /// stays behind every one of them.
    #[test]
    fn queued_requests_are_served_most_recently_drawn_first() {
        let mut slots = [Pslot::ZERO; PT_CAP];
        slots[3] = want(10, 100, true); // drawn long ago
        slots[9] = want(40, 400, true); // newest frame, drawn second
        slots[20] = want(40, 399, true); // newest frame, drawn first: reading order
        slots[1] = want(60, 5, false); // a lookahead/prefetch want, never drawn
        assert_eq!(next_wanted(&slots), Some(20));
        slots[20].state = P_LOADING;
        assert_eq!(next_wanted(&slots), Some(9));
        slots[9].state = P_LOADING;
        assert_eq!(next_wanted(&slots), Some(3), "an old drawn request still outranks speculation");
        slots[3].state = P_LOADING;
        assert_eq!(next_wanted(&slots), Some(1));
    }

    // ---- Lookahead (`Touch::Ahead`, `ahead_victim`) -------------------------------------------

    /// A settled lookahead entry with LRU age `use_`.
    fn ahead_slot(use_: c_uint) -> Pslot {
        Pslot { ahead: true, ..ready(use_, 0) }
    }

    #[test]
    fn a_lookahead_claim_is_flagged_aged_like_a_draw_and_adopted_by_the_draw_that_reaches_it() {
        let (_fresh, sid, _) = one_server();
        let path = key_for(sid, "/library/metadata/7001/thumb", 2, 2, 0);
        {
            let mut g = store();
            g.slots = [Pslot::ZERO; PT_CAP];
            g.frame = 5;
            g.clock = 10;
        }
        assert_eq!(lookup(sid, &path, Touch::Ahead).1, Warm::Claimed);
        {
            let g = store();
            let s = &g.slots[0];
            assert!(s.ahead && !s.visible && s.state == P_WANT);
            assert!(s.use_ > 10, "a fresh age, unlike a Warm's 0, so the next miss does not take it first");
            assert_ne!(s.frame, 5, "and no frame protection, so a Draw this frame still outranks it");
        }
        assert_eq!(lookup(sid, &path, Touch::Ahead).1, Warm::Known, "the second ask is a hit, not a claim");
        store().slots[0].state = P_READY;
        let (hit, _) = lookup(sid, &path, Touch::Draw);
        assert_eq!(hit, Some(PosterKey(0)), "the lookahead entry is exactly the slot the draw hits");
        let g = store();
        assert!(!g.slots[0].ahead && g.slots[0].visible && g.slots[0].frame == 5,
            "drawn once, it is an ordinary visible slot: no longer reclaimable as lookahead");
        drop(g);
        store().slots = [Pslot::ZERO; PT_CAP];
    }

    #[test]
    fn lookahead_keeps_the_prefetch_bound_and_never_revives_an_evicted_slot() {
        let (_fresh, sid, _) = one_server();
        let a = key_for(sid, "/library/metadata/7002/thumb", 2, 2, 0);
        let b = key_for(sid, "/library/metadata/7003/thumb", 2, 2, 0);
        store().slots = [Pslot::ZERO; PT_CAP];
        assert_eq!(lookup(sid, &a, Touch::Ahead).1, Warm::Claimed);
        assert_eq!(lookup(sid, &b, Touch::Ahead).1, Warm::Full,
            "one speculative fetch in flight spends the whole allowance, exactly as for a Warm");
        store().slots[0].state = P_EVICTED;
        assert_eq!(lookup(sid, &a, Touch::Ahead).1, Warm::Known);
        assert_eq!(store().slots[0].state, P_EVICTED, "only a Draw re-arms an evicted slot");
        store().slots = [Pslot::ZERO; PT_CAP];
    }

    /// **A visible miss always wins a slot.** Even a store with nothing but lookahead in it gives
    /// one up to a Draw (the oldest), because the Draw's victim rule counts a lookahead slot as an
    /// ordinary settled one.
    #[test]
    fn a_draw_miss_takes_a_slot_from_a_store_full_of_lookahead() {
        let (_fresh, sid, _) = one_server();
        let path = key_for(sid, "/library/metadata/7004/thumb", 2, 2, 0);
        {
            let mut g = store();
            g.slots = std::array::from_fn(|i| ahead_slot(100 + i as c_uint));
            g.slots[23].use_ = 3; // the oldest
            g.frame = 9;
        }
        let (hit, warm) = lookup(sid, &path, Touch::Draw);
        assert_eq!((hit, warm), (None, Warm::Claimed));
        let g = store();
        assert_eq!(g.slots[23].state, P_WANT, "the oldest lookahead entry went");
        assert!(g.slots[23].visible && !g.slots[23].ahead);
        assert_eq!(g.slots.iter().filter(|s| s.ahead).count(), PT_CAP - 1);
        drop(g);
        store().slots = [Pslot::ZERO; PT_CAP];
    }

    #[test]
    fn a_lookahead_claim_never_takes_a_slot_the_frame_drew_or_the_newest_ordinary_ones() {
        let f: c_uint = 20;
        // 32 ordinary slots drawn most recently (the cards on screen), 32 older ones.
        let mut slots: [Pslot; PT_CAP] = std::array::from_fn(|i| ready(1000 + i as c_uint, 3));
        for (i, s) in slots.iter_mut().enumerate().take(32) { s.use_ = 1 + i as c_uint; }
        assert_eq!(ahead_victim(&slots, f), Some(0), "the oldest of the unprotected half");
        // Protect: only the 32 newest are off limits, so a store with exactly 32 ordinary slots
        // and the rest lookahead offers no ordinary victim.
        let mut mixed: [Pslot; PT_CAP] = std::array::from_fn(|i| ahead_slot(1 + i as c_uint));
        for (i, s) in mixed.iter_mut().enumerate().take(32) { *s = ready(500 + i as c_uint, 3); }
        for s in mixed.iter_mut().skip(32).take(31) { s.frame = 0; }
        mixed[63] = Pslot { state: P_LOADING, ..ready(2, 0) }; // 31 settled lookahead + 1 ordinary in flight
        assert_eq!(ahead_victim(&mixed, f), None,
            "below the lookahead cap with every ordinary slot protected: no slot, the warming stops");
        // A drawn-this-frame slot is never a victim, protected or not.
        let drawn = [ready(1, f); PT_CAP];
        assert_eq!(ahead_victim(&drawn, f), None);
        // Empty first.
        let mut one_hole = [ready(1, 3); PT_CAP];
        one_hole[40] = Pslot::ZERO;
        assert_eq!(ahead_victim(&one_hole, f), Some(40));
    }

    /// At the cap the oldest LOOKAHEAD entry is recycled (a jump reclaims the old place's rows);
    /// below it, lookahead is never taken by lookahead.
    #[test]
    fn lookahead_recycles_its_own_oldest_only_at_the_cap() {
        let f: c_uint = 20;
        let mut slots: [Pslot; PT_CAP] = std::array::from_fn(|i| ready(1000 + i as c_uint, 3));
        for (i, s) in slots.iter_mut().enumerate().take(AHEAD_MAX) { *s = ahead_slot(50 + i as c_uint); }
        slots[11].use_ = 4;
        assert_eq!(ahead_victim(&slots, f), Some(11), "at the cap the oldest lookahead entry goes");
        // One lookahead entry short of the cap, with an old ordinary slot to take instead.
        slots[0] = ready(1, 3);
        assert_eq!(slots.iter().filter(|s| s.ahead).count(), AHEAD_MAX - 1);
        assert_eq!(ahead_victim(&slots, f), Some(0), "below the cap an ordinary old slot is taken, never a lookahead one");
    }

    /// **Two lookahead warms do not evict each other in a loop.** The store is simulated purely
    /// (a slot's identity rides in `pw`): a screenful of ordinary slots on top, stale slots below,
    /// then a lookahead set is asked for one card per "frame" until a frame asks for nothing.
    /// Then the user jumps and the SAME bounded process runs for an overlapping set.
    #[test]
    fn a_lookahead_set_converges_and_stops_even_across_a_jump() {
        const SET: c_int = tex::AHEAD_SET_MAX as c_int;
        let f: c_uint = 100;
        let mut clock: c_uint = 1000;
        let mut slots: [Pslot; PT_CAP] = std::array::from_fn(|i| Pslot {
            pw: -(i as c_int) - 1,
            ..ready(if i < 32 { 500 + i as c_uint } else { 1 + i as c_uint }, 3)
        });
        let on_screen: Vec<c_int> = (0..32).map(|i| -(i as c_int) - 1).collect();
        // One card per frame, nearest first (list order), until nothing is missing.
        let mut run = |slots: &mut [Pslot; PT_CAP], set: &[c_int]| -> usize {
            let mut claims = 0;
            for _frame in 0..500 {
                let Some(&want) = set.iter().find(|&&id| !slots.iter().any(|s| s.state != P_EMPTY && s.pw == id))
                else { return claims };
                let Some(at) = ahead_victim(slots, f) else { return claims };
                clock += 1;
                slots[at] = Pslot { pw: want, ahead: true, ..ready(clock, f - 1) };
                claims += 1;
            }
            panic!("the lookahead never converged: it is evicting itself");
        };
        let first: Vec<c_int> = (1..=SET).collect();
        assert_eq!(run(&mut slots, &first), SET as usize, "one claim per card, no re-claims");
        assert!(first.iter().all(|id| slots.iter().any(|s| s.pw == *id)), "the whole set is resident");
        // The jump: a half-overlapping set of the same size.
        let second: Vec<c_int> = (SET / 2 + 1..=SET / 2 + SET).collect();
        let claims = run(&mut slots, &second);
        assert!(claims <= SET as usize, "at most one claim per card of the new set; got {claims}");
        assert!(second.iter().all(|id| slots.iter().any(|s| s.pw == *id)), "the new set is resident");
        assert!(on_screen.iter().all(|id| slots.iter().any(|s| s.pw == *id)),
            "the cards on screen (the newest ordinary slots) were never recycled");
        // And a third ask for the same set claims nothing: the page can go idle.
        assert_eq!(run(&mut slots, &second), 0);
    }
}
