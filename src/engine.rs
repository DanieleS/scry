//! The **polling loop**: turn a resolved [`Profile`] into a stream of snapshots.
//!
//! This is the part of the engine that actually *watches* a game over time. It
//! is deliberately host-agnostic — it **produces** diffed snapshots and knows
//! nothing about clients, streaming, or any transport. Whatever imports `scry`
//! decides where the snapshots go.
//!
//! # Shape
//!
//! A [`Session`] is created once, via [`Session::attach`] (or the shorthand
//! [`Engine::attach`]). Attaching does the expensive work *once*: resolve each
//! Tier-1 watch's module base, run each Tier-2 watch's AOB scan, and cache the
//! resulting anchor addresses. The loop never scans per tick.
//!
//! Thereafter each [`Session::poll`] samples only the watches that are *due*
//! (per their own `rate_hz`), diffs the readings against the last known values,
//! and returns just what changed. A broken chain or failed read never yields a
//! garbage number — it surfaces as [`Value::Unavailable`]; and if every due read
//! fails for [`Config::reattach_after`] consecutive ticks, the session
//! re-resolves its anchors, the recovery path for a process that has moved out
//! from under it.
//!
//! # Driving it
//!
//! [`Session::poll`] is synchronous and takes the elapsed time explicitly, so a
//! caller (or a test) can drive it deterministically. For the common case,
//! [`Session::run`] spawns a dedicated, low-priority thread that ticks at
//! [`Config::base_tick`] and hands each non-empty diff to a callback:
//!
//! ```no_run
//! # use scry::{Engine, MemoryBackend};
//! # use scry::profile::Profile;
//! # fn demo<B: MemoryBackend + Send + 'static>(backend: B, profile: &Profile) {
//! use std::sync::mpsc;
//!
//! // A channel is just a callback that forwards — no separate API needed.
//! let (tx, rx) = mpsc::channel();
//! let _session = Engine::attach(backend, profile).run(move |diff| {
//!     let _ = tx.send(diff);
//! });
//! for diff in rx {
//!     // push `diff` to wherever the host wants it
//! }
//! # }
//! ```

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::aob;
use crate::backend::MemoryBackend;
use crate::profile::{Base, Profile, Rip, StringEncoding, StringLayout, ValueType, Watch};

/// A single sampled value — or the honest absence of one.
///
/// `Unavailable` is a first-class state, not an error: it means "this watch
/// could not be read this tick", and it diffs like any other value, so a
/// consumer learns the moment a field goes dark (and the moment it comes back).
///
/// Not `Copy`: [`Str`](Value::Str) and [`List`](Value::List) own heap data. The
/// engine clones a value only when it actually changes, so the cost lands on
/// real diffs, not on every quiet tick.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    I32(i32),
    U32(u32),
    F32(f32),
    U64(u64),
    /// A decoded string (per the watch's [`StringLayout`]). A null reference
    /// reads as the empty string, not `Unavailable`.
    Str(String),
    /// A [collection](crate::profile::Watch::Collection) sampled into an ordered
    /// array. Each element is itself a `Value`, so a broken element shows up as a
    /// nested [`Unavailable`](Value::Unavailable) without sinking the list, and
    /// the whole array diffs by equality like any scalar.
    List(Vec<Value>),
    /// The watch's chain could not be resolved or read this tick. The fail-soft
    /// state — never a stale or garbage number passed off as a live reading.
    Unavailable,
}

/// A diff: the labels whose value changed since they were last sampled, mapped
/// to their new values. `BTreeMap` keeps the order stable for testable,
/// reproducible output.
pub type Snapshot = BTreeMap<String, Value>;

/// Tuning for the polling loop.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// The loop's base cadence. Every watch is sampled at most this often; a
    /// watch's own `rate_hz` throttles it further. ~50–100 ms keeps the engine's
    /// overhead negligible against a host's capture/encode path.
    pub base_tick: Duration,
    /// Consecutive fully-failed ticks — every *due* watch unreadable — after
    /// which the session re-resolves module bases and re-scans anchors. Must be
    /// at least 1; the default is deliberately forgiving so a brief hiccup
    /// doesn't trigger a needless rescan.
    pub reattach_after: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            base_tick: Duration::from_millis(50),
            reattach_after: 10,
        }
    }
}

/// How a watch's anchor address is (re)found — the only per-tier difference,
/// captured once at attach so re-attach can repeat it verbatim.
enum AnchorKind {
    /// Tier-1: the anchor is the load base of this module.
    Module(String),
    /// Tier-2: the anchor is the first hit of this pre-parsed signature. If
    /// `rip` is set, the hit is a RIP-relative instruction and the real anchor is
    /// its decoded operand address, not the match address itself.
    Signature {
        pattern: Vec<aob::PatternByte>,
        rip: Option<Rip>,
    },
    /// The watch's anchor spec was itself invalid (an unparseable Tier-2
    /// signature). Kept so the label still exists, but it can never resolve —
    /// it reports `Unavailable` for the session's life.
    Invalid,
}

/// What a watch does once its anchor is resolved: read one typed value, or
/// iterate a container into an array. The per-tier difference (how the anchor is
/// *found*) lives in [`AnchorKind`]; this is the per-*kind* difference (what is
/// read once we have it).
enum Reader {
    /// A scalar [`Watch::Tier1`]/[`Watch::Tier2`]: walk `offsets` from the anchor
    /// and read one `ty` value.
    Scalar { offsets: Vec<i64>, ty: ValueType },
    /// A [`Watch::Collection`]: walk `base` from the anchor to the container,
    /// then iterate. Field meaning mirrors [`Watch::Collection`].
    Collection {
        base: Vec<i64>,
        count: Vec<i64>,
        items: Option<Vec<i64>>,
        first: i64,
        stride: i64,
        element: Vec<i64>,
        ty: ValueType,
        max: usize,
    },
}

/// A watch reduced to what the loop needs each tick, plus its schedule state.
struct Scheduled {
    name: String,
    kind: AnchorKind,
    reader: Reader,
    /// Minimum time between samples (`1 / rate_hz`); `ZERO` means every tick.
    period: Duration,
    /// Cached anchor, resolved at attach; `None` when it couldn't be found, in
    /// which case a re-attach retries it.
    anchor: Option<u64>,
    /// Elapsed time at/after which this watch is due to sample again.
    next_due: Duration,
}

/// Parse a Tier-2 signature into an [`AnchorKind`], recording an unparseable one
/// as [`AnchorKind::Invalid`] so it fails soft for the session's life rather
/// than re-erroring every tick.
fn signature_kind(anchor: &str, rip: Option<Rip>) -> AnchorKind {
    match aob::parse_pattern(anchor) {
        Ok(pattern) => AnchorKind::Signature { pattern, rip },
        Err(_) => AnchorKind::Invalid,
    }
}

/// Convert an optional rate into a minimum sampling period. A missing or
/// non-positive rate collapses to "every tick".
fn period_of(rate_hz: Option<f64>) -> Duration {
    match rate_hz {
        Some(hz) if hz > 0.0 => Duration::from_secs_f64(1.0 / hz),
        _ => Duration::ZERO,
    }
}

/// (Re)resolve a watch's anchor address against the live target. Any failure —
/// module not mapped, signature absent, region read error — becomes `None`
/// rather than aborting: one broken watch must not sink the others.
fn resolve_anchor<B: MemoryBackend + ?Sized>(backend: &B, kind: &AnchorKind) -> Option<u64> {
    match kind {
        AnchorKind::Module(name) => backend.module_base(name).ok(),
        AnchorKind::Signature { pattern, rip } => {
            let hit = aob::find_in_process(backend, pattern).ok().flatten()?;
            // A plain signature anchors at its match address; a RIP-relative one
            // decodes the instruction there into the operand address it names.
            match rip {
                Some(r) => backend.resolve_rip(hit, r.disp, r.len).ok(),
                None => Some(hit),
            }
        }
        AnchorKind::Invalid => None,
    }
}

/// Hard cap on the bytes read for one string — a garbage length or a missing
/// terminator can't drive an unbounded read. 1 KiB is ample for any name/label.
const STRING_MAX_BYTES: usize = 1024;

/// Read one typed value at an already-resolved address. Every read failure
/// becomes `Unavailable`, never a partial or guessed number.
fn read_typed<B: MemoryBackend + ?Sized>(backend: &B, addr: u64, ty: ValueType) -> Value {
    let read = match ty {
        ValueType::I32 => backend.read_i32(addr).map(Value::I32),
        ValueType::U32 => backend.read_u32(addr).map(Value::U32),
        ValueType::F32 => backend.read_f32(addr).map(Value::F32),
        ValueType::U64 => backend.read_u64(addr).map(Value::U64),
        ValueType::String(spec) => read_string(backend, addr, spec.layout()).map(Value::Str),
    };
    read.unwrap_or(Value::Unavailable)
}

/// Decode a string at `addr` per an engine-agnostic [`StringLayout`]. The layout
/// says everything: whether to dereference a reference first, where the length
/// (or a NUL terminator) is, the char offset, and the encoding. Nothing about
/// any one engine is hard-coded here — the profile carries the shape.
///
/// A null reference reads as `""` (honest empty, not a failure); a hard read
/// failure propagates so the watch surfaces `Unavailable` rather than a guess;
/// invalid code units decode lossily to U+FFFD.
fn read_string<B: MemoryBackend + ?Sized>(
    backend: &B,
    addr: u64,
    layout: StringLayout,
) -> crate::Result<String> {
    let object = if layout.deref {
        backend.read_ptr(addr)?
    } else {
        addr
    };
    if object == 0 {
        return Ok(String::new());
    }
    let unit = match layout.encoding {
        StringEncoding::Utf8 => 1usize,
        StringEncoding::Utf16 => 2usize,
    };
    let start = object.wrapping_add(layout.chars_at as u64);

    let bytes = match layout.len_at {
        // Length-prefixed (managed): a 32-bit count of code units at `len_at`.
        Some(off) => {
            let len = backend.read_i32(object.wrapping_add(off as u64))?;
            let want = (len.max(0) as usize)
                .saturating_mul(unit)
                .min(STRING_MAX_BYTES);
            if want == 0 {
                return Ok(String::new());
            }
            let mut buf = vec![0u8; want];
            backend.read_bytes(start, &mut buf)?;
            buf
        }
        // NUL-terminated (native/C): scan bounded blocks for an all-zero unit.
        None => read_until_nul(backend, start, unit)?,
    };

    Ok(match layout.encoding {
        StringEncoding::Utf8 => String::from_utf8_lossy(&bytes).into_owned(),
        StringEncoding::Utf16 => {
            let wide: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&wide)
        }
    })
}

/// Read a NUL-terminated payload from `start` in bounded blocks, stopping at the
/// first all-zero code `unit` or at [`STRING_MAX_BYTES`]. A read failure on the
/// *first* block propagates (the string is unreadable); a later partial read
/// ends the scan and returns what was collected — fail-soft, page-boundary safe.
fn read_until_nul<B: MemoryBackend + ?Sized>(
    backend: &B,
    start: u64,
    unit: usize,
) -> crate::Result<Vec<u8>> {
    const BLOCK: usize = 64;
    let mut out: Vec<u8> = Vec::new();
    while out.len() < STRING_MAX_BYTES {
        let take = BLOCK.min(STRING_MAX_BYTES - out.len());
        let mut buf = vec![0u8; take];
        if let Err(e) = backend.read_bytes(start + out.len() as u64, &mut buf) {
            if out.is_empty() {
                return Err(e);
            }
            break;
        }
        // Scan for an aligned terminator (a whole code unit of zero bytes).
        let mut cut = None;
        let mut i = 0;
        while i + unit <= buf.len() {
            if buf[i..i + unit].iter().all(|&b| b == 0) {
                cut = Some(i);
                break;
            }
            i += unit;
        }
        match cut {
            Some(c) => {
                out.extend_from_slice(&buf[..c]);
                break;
            }
            None => out.extend_from_slice(&buf),
        }
    }
    // Keep only whole code units.
    let keep = out.len() - (out.len() % unit);
    out.truncate(keep);
    Ok(out)
}

/// Sample one watch from its cached anchor. A scalar walks its chain and reads
/// one value; a collection iterates its container. Every failure path returns
/// `Unavailable` (or, per element, a nested `Unavailable`) — never a guess.
fn sample_one<B: MemoryBackend + ?Sized>(backend: &B, w: &Scheduled) -> Value {
    let anchor = match w.anchor {
        Some(a) => a,
        None => return Value::Unavailable,
    };
    match &w.reader {
        Reader::Scalar { offsets, ty } => match backend.resolve(anchor, offsets) {
            Ok(addr) => read_typed(backend, addr, *ty),
            Err(_) => Value::Unavailable,
        },
        Reader::Collection {
            base,
            count,
            items,
            first,
            stride,
            element,
            ty,
            max,
        } => {
            // Reach the container. A base that no longer resolves makes the whole
            // watch unavailable — there is nothing to iterate.
            let container = match backend.resolve(anchor, base) {
                Ok(a) => a,
                Err(_) => return Value::Unavailable,
            };
            // Size the list. If the count can't be read we can't know how many
            // elements to walk, so the whole watch is unavailable (a per-element
            // failure is different — that stays local to the element).
            let n = match backend
                .resolve(container, count)
                .and_then(|a| backend.read_i32(a))
            {
                Ok(raw) => (raw.max(0) as usize).min(*max),
                Err(_) => return Value::Unavailable,
            };
            // Find the element region: the backing array an `items` chain points
            // at (dereferenced), or the container itself for a bare pointer array.
            let region = match items {
                Some(items) => match backend
                    .resolve(container, items)
                    .and_then(|a| backend.read_ptr(a))
                {
                    Ok(array) => array,
                    Err(_) => return Value::Unavailable,
                },
                None => container,
            };
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                let slot = region
                    .wrapping_add(*first as u64)
                    .wrapping_add((i as u64).wrapping_mul(*stride as u64));
                let value = match backend.resolve(slot, element) {
                    Ok(addr) => read_typed(backend, addr, *ty),
                    Err(_) => Value::Unavailable,
                };
                out.push(value);
            }
            Value::List(out)
        }
    }
}

/// A live watch over a target process: attach once, poll repeatedly.
pub struct Session<B: MemoryBackend> {
    backend: B,
    config: Config,
    watches: Vec<Scheduled>,
    /// Last known value per label — the baseline every poll diffs against.
    last: BTreeMap<String, Value>,
    /// Consecutive fully-failed ticks; drives the re-attach decision.
    fail_streak: u32,
}

impl<B: MemoryBackend> Session<B> {
    /// Attach to the target behind `backend` for the given (already resolved)
    /// `profile`. Runs each watch's one-time anchor resolution — module bases
    /// and AOB scans — and caches the results.
    ///
    /// Infallible by design: a watch whose anchor can't be found now is kept and
    /// simply reports `Unavailable` until a re-attach recovers it. That keeps a
    /// single missing module or signature from denying telemetry for the rest.
    pub fn attach(backend: B, profile: &Profile, config: Config) -> Self {
        let mut watches = Vec::with_capacity(profile.watches.len());
        for w in &profile.watches {
            let (name, kind, reader, rate_hz) = match w {
                Watch::Tier1 {
                    name,
                    module,
                    offsets,
                    ty,
                    rate_hz,
                } => (
                    name.clone(),
                    AnchorKind::Module(module.clone()),
                    Reader::Scalar {
                        offsets: offsets.clone(),
                        ty: *ty,
                    },
                    *rate_hz,
                ),
                Watch::Tier2 {
                    name,
                    anchor,
                    rip,
                    offsets,
                    ty,
                    rate_hz,
                } => (
                    name.clone(),
                    // Parse the signature once. A malformed signature can never
                    // resolve, so record that rather than re-failing every tick.
                    signature_kind(anchor, *rip),
                    Reader::Scalar {
                        offsets: offsets.clone(),
                        ty: *ty,
                    },
                    *rate_hz,
                ),
                Watch::Collection {
                    name,
                    base,
                    count,
                    items,
                    first,
                    stride,
                    element,
                    ty,
                    max,
                    rate_hz,
                } => {
                    // A collection's base is anchored exactly like a scalar
                    // watch; only its `offsets` reach the container rather than a
                    // value. Everything after the base is the iteration recipe.
                    let (kind, base_offsets) = match base {
                        Base::Tier1 { module, offsets } => {
                            (AnchorKind::Module(module.clone()), offsets.clone())
                        }
                        Base::Tier2 {
                            anchor,
                            rip,
                            offsets,
                        } => (signature_kind(anchor, *rip), offsets.clone()),
                    };
                    (
                        name.clone(),
                        kind,
                        Reader::Collection {
                            base: base_offsets,
                            count: count.clone(),
                            items: items.clone(),
                            first: *first,
                            stride: *stride,
                            element: element.clone(),
                            ty: *ty,
                            max: *max,
                        },
                        *rate_hz,
                    )
                }
            };
            let anchor = resolve_anchor(&backend, &kind);
            watches.push(Scheduled {
                name,
                kind,
                reader,
                period: period_of(rate_hz),
                anchor,
                next_due: Duration::ZERO,
            });
        }
        Session {
            backend,
            config,
            watches,
            last: BTreeMap::new(),
            fail_streak: 0,
        }
    }

    /// Sample every watch due at `elapsed` (time since attach), diff against the
    /// last known values, and return only what changed.
    ///
    /// `elapsed` is supplied by the caller so the schedule is driven by a single
    /// monotonic clock — the threaded [`run`](Session::run) passes
    /// `start.elapsed()`; a test can pass exact instants. A watch is due when
    /// `elapsed` has reached its `next_due`; after sampling, its next due time is
    /// pushed out by its period.
    pub fn poll(&mut self, elapsed: Duration) -> Snapshot {
        let mut diff = Snapshot::new();
        let mut sampled = 0u32;
        let mut failed = 0u32;

        for w in &mut self.watches {
            if elapsed < w.next_due {
                continue;
            }
            sampled += 1;
            w.next_due = elapsed + w.period;

            let value = sample_one(&self.backend, w);
            if value == Value::Unavailable {
                failed += 1;
            }

            // Emit only genuine changes. A first sighting (no prior value) always
            // counts as a change, including a first-seen `Unavailable`. The clone
            // lands only on an actual change — a quiet tick copies nothing.
            let changed = self.last.get(&w.name) != Some(&value);
            if changed {
                self.last.insert(w.name.clone(), value.clone());
                diff.insert(w.name.clone(), value);
            }
        }

        // Re-attach bookkeeping runs only on ticks that actually sampled
        // something: a quiet tick (nothing due) is neither success nor failure.
        if sampled > 0 {
            if failed == sampled {
                self.fail_streak += 1;
                if self.fail_streak >= self.config.reattach_after.max(1) {
                    self.reattach();
                    self.fail_streak = 0;
                }
            } else {
                self.fail_streak = 0;
            }
        }

        diff
    }

    /// Re-resolve every watch's anchor against the live target. Called when a
    /// run of fully-failed ticks suggests the process moved (relocated module,
    /// freed region) — the same resolution attach did, repeated.
    fn reattach(&mut self) {
        for w in &mut self.watches {
            w.anchor = resolve_anchor(&self.backend, &w.kind);
        }
    }

    /// The last known value of every label sampled so far — the full state a
    /// diff stream is relative to. Useful for a consumer that joins late and
    /// needs the current picture, not just the next change.
    pub fn current(&self) -> &BTreeMap<String, Value> {
        &self.last
    }
}

impl<B: MemoryBackend + Send + 'static> Session<B> {
    /// Drive this session on a dedicated, low-priority background thread,
    /// delivering each non-empty diff to `sink` as it ticks at
    /// [`Config::base_tick`].
    ///
    /// The returned [`Handle`] owns the thread; dropping it (or calling
    /// [`Handle::stop`]) signals the loop to finish its current tick and joins.
    /// The thread lowers its own scheduling priority so telemetry never competes
    /// with a host's capture/encode path.
    pub fn run<F>(mut self, mut sink: F) -> Handle
    where
        F: FnMut(Snapshot) + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let base_tick = self.config.base_tick;

        let join = std::thread::spawn(move || {
            lower_thread_priority();
            let start = Instant::now();
            while !stop_thread.load(Ordering::Relaxed) {
                let diff = self.poll(start.elapsed());
                if !diff.is_empty() {
                    sink(diff);
                }
                std::thread::sleep(base_tick);
            }
        });

        Handle {
            stop,
            join: Some(join),
        }
    }
}

/// The headline entry point: attach with default [`Config`].
pub struct Engine;

impl Engine {
    /// Attach to `backend` for `profile` using [`Config::default`]. The returned
    /// [`Session`] can be polled directly or handed to [`Session::run`].
    pub fn attach<B: MemoryBackend>(backend: B, profile: &Profile) -> Session<B> {
        Session::attach(backend, profile, Config::default())
    }
}

/// Owns a running [`Session::run`] thread. Stops and joins it on drop.
pub struct Handle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Handle {
    /// Signal the loop to stop and wait for the thread to finish. Idempotent;
    /// also invoked automatically when the handle is dropped.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Best-effort nudge of the current thread to a lower scheduling priority.
///
/// On Linux `setpriority(PRIO_PROCESS, 0, …)` applies to the calling thread, so
/// this quietly de-prioritises the telemetry loop. Failure is ignored: a
/// slightly-too-eager background thread is a nuisance, never a correctness bug.
#[cfg(unix)]
fn lower_thread_priority() {
    extern "C" {
        fn setpriority(which: i32, who: u32, prio: i32) -> i32;
    }
    const PRIO_PROCESS: i32 = 0;
    const NICE: i32 = 10;
    unsafe {
        let _ = setpriority(PRIO_PROCESS, 0, NICE);
    }
}

#[cfg(not(unix))]
fn lower_thread_priority() {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Region;
    use crate::error::{Error, Result};
    use crate::profile::{Match, StringPreset, StringSpec};
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;

    /// The IL2CPP string type, as a preset — the shape the fixtures plant.
    fn il2cpp_string() -> ValueType {
        ValueType::String(StringSpec::Preset(StringPreset::Il2cpp))
    }

    /// A deterministic in-memory backend with interior mutability, so a test can
    /// mutate the "game's" memory between polls, toggle a total read failure, and
    /// count reads per address and module-base lookups (to observe re-attach).
    ///
    /// Implemented on `Rc<Fake>` so a test can keep a clone to poke while the
    /// `Session` owns another — mirroring how the real world hands the backend to
    /// the loop and mutates the game from the outside. Single-threaded only
    /// (`Cell`/`RefCell`); the threaded `run` path is exercised against the cavia
    /// in the integration tests instead.
    struct Fake {
        base: u64,
        mem: RefCell<Vec<u8>>,
        reads_at: RefCell<HashMap<u64, u32>>,
        base_calls: Cell<u32>,
        fail: Cell<bool>,
    }

    impl Fake {
        fn new(len: usize) -> Self {
            Fake {
                base: 0x4000_0000,
                mem: RefCell::new(vec![0u8; len]),
                reads_at: RefCell::new(HashMap::new()),
                base_calls: Cell::new(0),
                fail: Cell::new(false),
            }
        }

        fn write_i32(&self, off: usize, v: i32) {
            self.mem.borrow_mut()[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }

        fn write_u64(&self, off: usize, v: u64) {
            self.mem.borrow_mut()[off..off + 8].copy_from_slice(&v.to_le_bytes());
        }

        fn write_bytes(&self, off: usize, bytes: &[u8]) {
            self.mem.borrow_mut()[off..off + bytes.len()].copy_from_slice(bytes);
        }

        fn reads_at(&self, addr: u64) -> u32 {
            self.reads_at.borrow().get(&addr).copied().unwrap_or(0)
        }
    }

    impl MemoryBackend for Rc<Fake> {
        fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> Result<()> {
            *self.reads_at.borrow_mut().entry(addr).or_insert(0) += 1;
            if self.fail.get() {
                return Err(Error::ShortRead {
                    expected: buf.len(),
                    got: 0,
                });
            }
            let mem = self.mem.borrow();
            let start = addr
                .checked_sub(self.base)
                .filter(|s| {
                    (*s as usize)
                        .checked_add(buf.len())
                        .is_some_and(|e| e <= mem.len())
                })
                .ok_or(Error::ShortRead {
                    expected: buf.len(),
                    got: 0,
                })? as usize;
            buf.copy_from_slice(&mem[start..start + buf.len()]);
            Ok(())
        }

        fn module_base(&self, _name: &str) -> Result<u64> {
            self.base_calls.set(self.base_calls.get() + 1);
            if self.fail.get() {
                return Err(Error::ModuleNotFound("fake".to_string()));
            }
            Ok(self.base)
        }

        fn readable_regions(&self) -> Result<Vec<Region>> {
            Ok(vec![Region {
                start: self.base,
                len: self.mem.borrow().len() as u64,
            }])
        }
    }

    /// A minimal profile identity — the polling tests attach directly, so the
    /// `match` block is never actually exercised here.
    fn ident() -> Match {
        Match {
            process: "fake".to_string(),
            module: "fake".to_string(),
            version: None,
            probe: "90".to_string(),
        }
    }

    /// A Tier-1 watch reading a single i32 at `module_base + off` (one-element
    /// chain: no intermediate deref, so each sample is exactly one read).
    fn tier1(name: &str, off: i64, rate_hz: Option<f64>) -> Watch {
        Watch::Tier1 {
            name: name.to_string(),
            module: "fake".to_string(),
            offsets: vec![off],
            ty: ValueType::I32,
            rate_hz,
        }
    }

    #[test]
    fn first_poll_reports_the_value_then_only_changes() {
        let fake = Rc::new(Fake::new(64));
        fake.write_i32(0, 100);
        let profile = Profile {
            label: None,
            match_: ident(),
            watches: vec![tier1("hp", 0, None)],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());

        // First poll: unknown -> 100, so it is emitted.
        let d = s.poll(Duration::ZERO);
        assert_eq!(d.get("hp"), Some(&Value::I32(100)));

        // No change -> empty diff.
        assert!(s.poll(Duration::from_millis(50)).is_empty());

        // Mutate the "game" from the outside; the change is emitted, once.
        fake.write_i32(0, 250);
        let d = s.poll(Duration::from_millis(100));
        assert_eq!(d.get("hp"), Some(&Value::I32(250)));
        assert!(s.poll(Duration::from_millis(150)).is_empty());
    }

    #[test]
    fn per_watch_rate_throttles_sampling() {
        let fake = Rc::new(Fake::new(64));
        let profile = Profile {
            label: None,
            match_: ident(),
            watches: vec![
                tier1("fast", 0, Some(20.0)), // 50 ms period -> every tick
                tier1("slow", 8, Some(2.0)),  // 500 ms period -> every 10th tick
            ],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());

        // Drive 40 ticks of 50 ms: elapsed = 0, 50, …, 1950 ms.
        for i in 0..40u64 {
            s.poll(Duration::from_millis(i * 50));
        }

        let fast_addr = fake.base; // offset 0
        let slow_addr = fake.base + 8;
        assert_eq!(
            fake.reads_at(fast_addr),
            40,
            "fast watch sampled every tick"
        );
        assert_eq!(
            fake.reads_at(slow_addr),
            4,
            "slow watch sampled at 0/500/1000/1500 ms only"
        );
    }

    #[test]
    fn broken_read_surfaces_unavailable_and_recovers() {
        let fake = Rc::new(Fake::new(64));
        fake.write_i32(0, 7);
        let profile = Profile {
            label: None,
            match_: ident(),
            watches: vec![tier1("hp", 0, None)],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());

        // Healthy read first.
        assert_eq!(s.poll(Duration::ZERO).get("hp"), Some(&Value::I32(7)));

        // Go dark: the next poll flips the watch to Unavailable (emitted once),
        // and stays quiet while it remains dark.
        fake.fail.set(true);
        assert_eq!(
            s.poll(Duration::from_millis(50)).get("hp"),
            Some(&Value::Unavailable),
            "a failed read must surface as Unavailable, never a garbage value"
        );
        assert!(s.poll(Duration::from_millis(100)).is_empty());

        // Recover: once reads succeed again the value returns and is emitted.
        fake.write_i32(0, 9);
        fake.fail.set(false);
        // May take a few ticks: recovery flows through a re-attach that re-resolves
        // the anchor. Poll until the value comes back.
        let mut recovered = None;
        for i in 3..40u64 {
            if let Some(v) = s.poll(Duration::from_millis(i * 50)).get("hp") {
                recovered = Some(v.clone());
                break;
            }
        }
        assert_eq!(
            recovered,
            Some(Value::I32(9)),
            "watch should recover its value"
        );
    }

    #[test]
    fn total_failure_triggers_reattach() {
        let fake = Rc::new(Fake::new(64));
        fake.write_i32(0, 1);
        let profile = Profile {
            label: None,
            match_: ident(),
            watches: vec![tier1("hp", 0, None)],
        };
        let config = Config {
            base_tick: Duration::from_millis(50),
            reattach_after: 3,
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, config);

        // Attach resolved the module base exactly once.
        assert_eq!(fake.base_calls.get(), 1);

        // One good tick, then fail everything.
        s.poll(Duration::ZERO);
        fake.fail.set(true);

        // Three consecutive fully-failed ticks must trigger exactly one re-attach
        // (one more module_base lookup). Before the third, none yet.
        s.poll(Duration::from_millis(50));
        s.poll(Duration::from_millis(100));
        assert_eq!(
            fake.base_calls.get(),
            1,
            "no re-attach before the threshold"
        );
        s.poll(Duration::from_millis(150));
        assert_eq!(
            fake.base_calls.get(),
            2,
            "the Nth fully-failed tick must re-resolve anchors"
        );
    }

    #[test]
    fn quiet_ticks_do_not_count_toward_reattach() {
        // A watch that is not due contributes neither success nor failure, so a
        // stretch of quiet ticks must never be mistaken for total failure.
        let fake = Rc::new(Fake::new(64));
        let profile = Profile {
            label: None,
            match_: ident(),
            watches: vec![tier1("slow", 0, Some(1.0))], // 1 s period
        };
        let config = Config {
            base_tick: Duration::from_millis(50),
            reattach_after: 2,
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, config);
        s.poll(Duration::ZERO); // samples once (succeeds), next due at 1 s
        fake.fail.set(true);

        // Many ticks before the watch is due again: all quiet, none count.
        for i in 1..15u64 {
            s.poll(Duration::from_millis(i * 50));
        }
        assert_eq!(
            fake.base_calls.get(),
            1,
            "quiet (not-due) ticks must not accrue a failure streak"
        );
    }

    #[test]
    fn tier2_rip_relative_resolves_static_pointer() {
        // Reproduce the x64 static-base shape end-to-end through the engine:
        //   a `mov rax, [rip+disp32]` instruction whose operand is a static slot
        //   (PLAYER) holding a pointer to a struct (STATS) whose first field is hp.
        // The Tier-2 watch AOB-scans the instruction, decodes the displacement to
        // reach PLAYER, then walks [deref PLAYER -> STATS, +0 -> hp].
        let fake = Rc::new(Fake::new(256));
        let base = fake.base;

        // Layout inside the fake's single region.
        let stub_off = 0x10usize; // the `mov rax, [rip+disp32]` bytes
        let player_off = 0x80usize; // static slot holding a pointer
        let stats_off = 0xC0usize; // the "heap" struct; hp at its start

        // PLAYER holds the absolute address of STATS; STATS.hp = 4242.
        fake.write_u64(player_off, base + stats_off as u64);
        fake.write_i32(stats_off, 4242);

        // Build `48 8B 05 <disp32>` + a unique tail so the scan is unambiguous.
        // disp32 is chosen so that anchor + 7 + disp32 == address of PLAYER.
        let anchor = base + stub_off as u64;
        let player_addr = base + player_off as u64;
        let disp32 = (player_addr as i64 - (anchor as i64 + 7)) as i32;
        let mut stub = vec![0x48u8, 0x8B, 0x05];
        stub.extend_from_slice(&disp32.to_le_bytes());
        stub.extend_from_slice(&[0xC3, 0x90, 0x5A, 0xA5]); // ret; nop; unique marker
        fake.write_bytes(stub_off, &stub);

        let profile = Profile {
            label: None,
            match_: ident(),
            watches: vec![Watch::Tier2 {
                name: "hp".to_string(),
                anchor: "48 8B 05 ?? ?? ?? ?? C3 90 5A A5".to_string(),
                rip: Some(Rip { disp: 3, len: 7 }),
                offsets: vec![0, 0],
                ty: ValueType::I32,
                rate_hz: None,
            }],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());

        assert_eq!(
            s.poll(Duration::ZERO).get("hp"),
            Some(&Value::I32(4242)),
            "RIP-relative Tier-2 watch must decode the displacement and read hp"
        );
    }

    #[test]
    fn tier2_without_rip_anchors_at_the_match() {
        // The regression guard for the default path: a Tier-2 watch with no `rip`
        // block still treats the AOB hit itself as the chain start. Here the
        // signature bytes double as the value's storage (hp read straight from the
        // matched region), so offsets is empty.
        let fake = Rc::new(Fake::new(64));
        // Plant a unique marker whose first 4 bytes also read as the i32 0x11223344.
        fake.write_bytes(0x8, &[0x44, 0x33, 0x22, 0x11, 0x5A, 0xA5, 0x5A, 0xA5]);

        let profile = Profile {
            label: None,
            match_: ident(),
            watches: vec![Watch::Tier2 {
                name: "marker".to_string(),
                anchor: "44 33 22 11 5A A5 5A A5".to_string(),
                rip: None,
                offsets: vec![],
                ty: ValueType::U32,
                rate_hz: None,
            }],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());
        assert_eq!(
            s.poll(Duration::ZERO).get("marker"),
            Some(&Value::U32(0x1122_3344)),
            "a rip-less Tier-2 watch must anchor at the match address"
        );
    }

    /// Lay out a C#-`List<T>`-shaped container in a `Fake`, returning the watch
    /// that reads it. Container at 0x100 (items ptr @+0, count @+8); backing
    /// array at 0x200 with a 0x20 header then `stride`-8 pointer slots; each slot
    /// points at a 4-byte element. `hps` supplies both the element values and the
    /// stored count.
    fn plant_collection(fake: &Fake, hps: &[i32]) {
        let base = fake.base;
        fake.write_u64(0x100, base + 0x200); // items -> backing array
        fake.write_i32(0x108, hps.len() as i32); // count
        for (i, &hp) in hps.iter().enumerate() {
            let elem_off = 0x300 + i * 0x40;
            fake.write_u64(0x220 + i * 8, base + elem_off as u64); // slot -> element
            fake.write_i32(elem_off, hp);
        }
    }

    fn i32_collection_watch(name: &str, max: usize) -> Watch {
        Watch::Collection {
            name: name.to_string(),
            base: Base::Tier1 {
                module: "fake".to_string(),
                offsets: vec![0x100],
            },
            count: vec![0x8],
            items: Some(vec![0x0]),
            first: 0x20,
            stride: 8,
            element: vec![0, 0], // slot -> deref -> element base
            ty: ValueType::I32,
            max,
            rate_hz: None,
        }
    }

    #[test]
    fn collection_reads_an_ordered_typed_array() {
        let fake = Rc::new(Fake::new(0x600));
        plant_collection(&fake, &[11, 22, 33]);
        let profile = Profile {
            label: None,
            match_: ident(),
            watches: vec![i32_collection_watch("enemy_hp", 64)],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());

        let d = s.poll(Duration::ZERO);
        assert_eq!(
            d.get("enemy_hp"),
            Some(&Value::List(vec![
                Value::I32(11),
                Value::I32(22),
                Value::I32(33)
            ])),
            "a collection must emit its elements in order"
        );
        // Unchanged list -> quiet tick.
        assert!(s.poll(Duration::from_millis(50)).is_empty());

        // Mutate one element from the outside; the whole array re-diffs, once.
        fake.write_i32(0x340, 99);
        let d = s.poll(Duration::from_millis(100));
        assert_eq!(
            d.get("enemy_hp"),
            Some(&Value::List(vec![
                Value::I32(11),
                Value::I32(99),
                Value::I32(33)
            ]))
        );
    }

    #[test]
    fn collection_count_is_clamped_to_max() {
        // A bogus count (here a truthful 3, but max is 2) can never walk past the
        // cap — the guard against a garbage count looping unboundedly.
        let fake = Rc::new(Fake::new(0x600));
        plant_collection(&fake, &[11, 22, 33]);
        let profile = Profile {
            label: None,
            match_: ident(),
            watches: vec![i32_collection_watch("capped", 2)],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());
        assert_eq!(
            s.poll(Duration::ZERO).get("capped"),
            Some(&Value::List(vec![Value::I32(11), Value::I32(22)])),
            "count must be clamped to max"
        );
    }

    #[test]
    fn collection_element_fails_soft_without_sinking_the_list() {
        let fake = Rc::new(Fake::new(0x600));
        plant_collection(&fake, &[11, 22, 33]);
        // Point the middle slot at an unmapped address: its element must read as
        // a nested Unavailable while its neighbours stay live.
        fake.write_u64(0x228, 0xdead_0000);
        let profile = Profile {
            label: None,
            match_: ident(),
            watches: vec![i32_collection_watch("enemy_hp", 64)],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());
        assert_eq!(
            s.poll(Duration::ZERO).get("enemy_hp"),
            Some(&Value::List(vec![
                Value::I32(11),
                Value::Unavailable,
                Value::I32(33)
            ])),
            "a broken element is Unavailable in place, never sinking the list"
        );
    }

    #[test]
    fn collection_unreadable_count_is_wholly_unavailable() {
        // No memory planted: the count read fails, so the list can't be sized and
        // the whole watch is Unavailable (not an empty list, which would be a lie).
        let fake = Rc::new(Fake::new(0x10));
        let profile = Profile {
            label: None,
            match_: ident(),
            watches: vec![i32_collection_watch("enemy_hp", 64)],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());
        assert_eq!(
            s.poll(Duration::ZERO).get("enemy_hp"),
            Some(&Value::Unavailable)
        );
    }

    #[test]
    fn reads_an_il2cpp_string_value() {
        // Plant a System.String object (len @+0x10, utf16 @+0x14) at 0x40 and a
        // slot at 0x10 holding a reference to it — the shape a string field takes.
        let fake = Rc::new(Fake::new(0x100));
        let base = fake.base;
        fake.write_u64(0x10, base + 0x40); // slot -> string object
        fake.write_i32(0x50, 4); // object+0x10 = length
        let utf16: Vec<u8> = "ZALE".encode_utf16().flat_map(u16::to_le_bytes).collect();
        fake.write_bytes(0x54, &utf16); // object+0x14 = payload

        let profile = Profile {
            label: None,
            match_: ident(),
            watches: vec![Watch::Tier1 {
                name: "name".to_string(),
                module: "fake".to_string(),
                offsets: vec![0x10], // resolve to the reference slot; read_string derefs
                ty: il2cpp_string(),
                rate_hz: None,
            }],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());
        assert_eq!(
            s.poll(Duration::ZERO).get("name"),
            Some(&Value::Str("ZALE".to_string())),
            "a string watch must decode the referenced System.String"
        );
    }

    #[test]
    fn reads_a_native_nul_terminated_utf8_string() {
        // Not IL2CPP: an inline (no deref) NUL-terminated UTF-8 C string — the
        // de-biased path. Proves the runtime reads a layout from the profile, not
        // a baked-in engine shape.
        let fake = Rc::new(Fake::new(0x100));
        fake.write_bytes(0x20, b"GARL\0extra"); // terminator ends it at "GARL"

        let layout = StringLayout {
            encoding: StringEncoding::Utf8,
            len_at: None, // NUL-terminated
            chars_at: 0,
            deref: false, // the address is the buffer itself
        };
        let profile = Profile {
            label: None,
            match_: ident(),
            watches: vec![Watch::Tier1 {
                name: "tag".to_string(),
                module: "fake".to_string(),
                offsets: vec![0x20], // resolve straight to the buffer
                ty: ValueType::String(StringSpec::Layout(layout)),
                rate_hz: None,
            }],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());
        assert_eq!(
            s.poll(Duration::ZERO).get("tag"),
            Some(&Value::Str("GARL".to_string())),
            "a NUL-terminated UTF-8 layout must read a native string"
        );
    }

    #[test]
    fn null_string_reference_reads_empty_not_unavailable() {
        let fake = Rc::new(Fake::new(0x100));
        // Slot at 0x10 holds a null reference (zeroed memory).
        let profile = Profile {
            label: None,
            match_: ident(),
            watches: vec![Watch::Tier1 {
                name: "name".to_string(),
                module: "fake".to_string(),
                offsets: vec![0x10],
                ty: il2cpp_string(),
                rate_hz: None,
            }],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());
        assert_eq!(
            s.poll(Duration::ZERO).get("name"),
            Some(&Value::Str(String::new())),
            "a null string reference is an empty string, not Unavailable"
        );
    }

    #[test]
    fn malformed_signature_watch_is_permanently_unavailable() {
        let fake = Rc::new(Fake::new(64));
        let profile = Profile {
            label: None,
            match_: ident(),
            watches: vec![Watch::Tier2 {
                name: "bad".to_string(),
                anchor: "not hex".to_string(),
                rip: None,
                offsets: vec![0],
                ty: ValueType::I32,
                rate_hz: None,
            }],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());
        assert_eq!(
            s.poll(Duration::ZERO).get("bad"),
            Some(&Value::Unavailable),
            "an unparseable signature never resolves, but must not panic"
        );
    }
}
