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
//! # use scry::{Engine, LinuxBackend};
//! # use scry::profile::Profile;
//! # fn demo(backend: LinuxBackend, profile: &Profile) {
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
use crate::profile::{Profile, ValueType, Watch};

/// A single sampled value — or the honest absence of one.
///
/// `Unavailable` is a first-class state, not an error: it means "this watch
/// could not be read this tick", and it diffs like any other value, so a
/// consumer learns the moment a field goes dark (and the moment it comes back).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    I32(i32),
    U32(u32),
    F32(f32),
    U64(u64),
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
    /// Tier-2: the anchor is the first hit of this pre-parsed signature.
    Signature(Vec<aob::PatternByte>),
    /// The watch's anchor spec was itself invalid (an unparseable Tier-2
    /// signature). Kept so the label still exists, but it can never resolve —
    /// it reports `Unavailable` for the session's life.
    Invalid,
}

/// A watch reduced to what the loop needs each tick, plus its schedule state.
struct Scheduled {
    name: String,
    kind: AnchorKind,
    offsets: Vec<i64>,
    ty: ValueType,
    /// Minimum time between samples (`1 / rate_hz`); `ZERO` means every tick.
    period: Duration,
    /// Cached anchor, resolved at attach; `None` when it couldn't be found, in
    /// which case a re-attach retries it.
    anchor: Option<u64>,
    /// Elapsed time at/after which this watch is due to sample again.
    next_due: Duration,
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
        AnchorKind::Signature(pattern) => aob::find_in_process(backend, pattern).ok().flatten(),
        AnchorKind::Invalid => None,
    }
}

/// Sample one watch: walk its chain from the cached anchor and read the typed
/// value. Every failure path returns `Unavailable`, never a partial or guessed
/// number.
fn sample_one<B: MemoryBackend + ?Sized>(backend: &B, w: &Scheduled) -> Value {
    let anchor = match w.anchor {
        Some(a) => a,
        None => return Value::Unavailable,
    };
    let addr = match backend.resolve(anchor, &w.offsets) {
        Ok(a) => a,
        Err(_) => return Value::Unavailable,
    };
    let read = match w.ty {
        ValueType::I32 => backend.read_i32(addr).map(Value::I32),
        ValueType::U32 => backend.read_u32(addr).map(Value::U32),
        ValueType::F32 => backend.read_f32(addr).map(Value::F32),
        ValueType::U64 => backend.read_u64(addr).map(Value::U64),
    };
    read.unwrap_or(Value::Unavailable)
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
            let (name, kind, offsets, ty, rate_hz) = match w {
                Watch::Tier1 {
                    name,
                    module,
                    offsets,
                    ty,
                    rate_hz,
                } => (
                    name.clone(),
                    AnchorKind::Module(module.clone()),
                    offsets.clone(),
                    *ty,
                    *rate_hz,
                ),
                Watch::Tier2 {
                    name,
                    anchor,
                    offsets,
                    ty,
                    rate_hz,
                } => {
                    // Parse the signature once. A malformed signature can never
                    // resolve, so record that rather than re-failing every tick.
                    let kind = match aob::parse_pattern(anchor) {
                        Ok(pattern) => AnchorKind::Signature(pattern),
                        Err(_) => AnchorKind::Invalid,
                    };
                    (name.clone(), kind, offsets.clone(), *ty, *rate_hz)
                }
            };
            let anchor = resolve_anchor(&backend, &kind);
            watches.push(Scheduled {
                name,
                kind,
                offsets,
                ty,
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
            // counts as a change, including a first-seen `Unavailable`.
            let changed = self.last.get(&w.name) != Some(&value);
            if changed {
                self.last.insert(w.name.clone(), value);
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
    use crate::profile::Match;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;

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
                recovered = Some(*v);
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
    fn malformed_signature_watch_is_permanently_unavailable() {
        let fake = Rc::new(Fake::new(64));
        let profile = Profile {
            label: None,
            match_: ident(),
            watches: vec![Watch::Tier2 {
                name: "bad".to_string(),
                anchor: "not hex".to_string(),
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
