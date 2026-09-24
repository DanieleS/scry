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

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::aob;
use crate::backend::MemoryBackend;
use crate::profile::{
    Base, Clause, Compare, Expr, Extremum, Field, Fold, Literal, Profile, Rip, StringEncoding,
    StringLayout, ValueType, Watch,
};

/// A single sampled value — or the honest absence of one.
///
/// `Unavailable` is a first-class state, not an error: it means "this watch
/// could not be read this tick", and it diffs like any other value, so a
/// consumer learns the moment a field goes dark (and the moment it comes back).
///
/// Not `Copy`: [`Str`](Value::Str) and [`List`](Value::List) own heap data. The
/// engine clones a value only when it actually changes, so the cost lands on
/// real diffs, not on every quiet tick.
///
/// Equality is what the diff runs on, so it is defined by hand rather than
/// derived: see the [`PartialEq`] impl for how floats compare.
#[derive(Debug, Clone)]
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
    /// A [record](crate::profile::Watch::Record) — named fields read off a shared
    /// base — or a record-valued [collection](crate::profile::Watch::Collection)
    /// element. Keys are ordered (`BTreeMap`) for stable, testable output; each
    /// field is itself a `Value`, so a broken field is a nested
    /// [`Unavailable`](Value::Unavailable) without sinking the record. This is the
    /// engine's *one* level of structure: the same `label → Value` shape as a
    /// snapshot, nested once — no deeper entity modelling.
    Map(BTreeMap<String, Value>),
    /// The watch's chain could not be resolved or read this tick. The fail-soft
    /// state — never a stale or garbage number passed off as a live reading.
    Unavailable,
}

/// Equality as the diff needs it: "would a consumer see the same reading".
///
/// A derived `PartialEq` compares an [`F32`](Value::F32) with IEEE `==`, under
/// which `NaN != NaN`. A torn or uninitialised float in a game's memory is very
/// often a NaN, and with IEEE equality the diff would call it changed on every
/// single tick and resend it — together with the whole list or record that
/// holds it. So floats compare by their bits instead, with every NaN equal to
/// every other (they all reach the wire as the same `null`). The one visible
/// consequence is that `0.0` and `-0.0` now differ, which is honest: they
/// serialise differently too.
///
/// Comparing bits also makes the relation reflexive, which is what lets
/// `Value` be [`Eq`].
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::I32(a), Value::I32(b)) => a == b,
            (Value::U32(a), Value::U32(b)) => a == b,
            (Value::F32(a), Value::F32(b)) => {
                a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
            }
            (Value::U64(a), Value::U64(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Map(a), Value::Map(b)) => a == b,
            (Value::Unavailable, Value::Unavailable) => true,
            _ => false,
        }
    }
}

impl Eq for Value {}

/// The **wire form** of a value: the shape a host outside this process sees.
///
/// Deliberately *untagged* — a number serialises as a bare JSON number, a string
/// as a string, a list as an array, a record as an object, and
/// [`Unavailable`](Value::Unavailable) as `null`. The variant name is not on the
/// wire because the consumer already knows it: the watch's `type` is declared in
/// the profile it loaded. Tagging every reading (`{"I32": 42}`) would put that
/// same fact on every line of a stream that emits one per tick.
///
/// This is a **compatibility surface**. Once a host parses it, the mapping below
/// cannot change without breaking that host, so it is defined here — in the
/// engine — rather than left to each host to invent.
///
/// One wrinkle worth knowing: a non-finite `f32` (a `NaN` read out of a game
/// mid-write) has no JSON representation and serialises as `null`, i.e. it is
/// indistinguishable from `Unavailable`. Both mean "no meaningful reading this
/// tick", so the collapse is honest rather than lossy.
impl serde::Serialize for Value {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::I32(n) => s.serialize_i32(*n),
            Value::U32(n) => s.serialize_u32(*n),
            Value::F32(x) => s.serialize_f32(*x),
            Value::U64(n) => s.serialize_u64(*n),
            Value::Str(v) => s.serialize_str(v),
            // Both delegate: `Vec<Value>`/`BTreeMap<String, Value>` are already
            // serialisable once `Value` is, so nesting comes out right for free.
            Value::List(items) => items.serialize(s),
            Value::Map(fields) => fields.serialize(s),
            Value::Unavailable => s.serialize_none(),
        }
    }
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

/// A record's fields, flattened for the loop: `(name, offsets, ty)` per field,
/// resolved relative to the record's shared base. A `Vec` (not a map) keeps the
/// attach-time order; the emitted [`Value::Map`] re-keys by name for stable
/// output. Built once from a profile's [`Field`] map.
type RecordFields = Vec<(String, Vec<i64>, ValueType)>;

/// Flatten a profile's `name → Field` map into the loop's [`RecordFields`].
fn flatten_fields(fields: &std::collections::BTreeMap<String, Field>) -> RecordFields {
    fields
        .iter()
        .map(|(name, f)| (name.clone(), f.offsets.clone(), f.ty))
        .collect()
}

/// What a collection's element is: a single typed value, or a record of named
/// fields read relative to the element's resolved address. Mirrors the
/// scalar-XOR-record choice on [`Watch::Collection`].
enum ElementReader {
    /// A scalar element: read one `ty` value at the element address.
    Scalar(ValueType),
    /// A record element: read these fields relative to the element address.
    Record(RecordFields),
}

/// What a watch does once its anchor is resolved: read one typed value, iterate
/// a container into an array, or read named fields off a base. The per-tier
/// difference (how the anchor is *found*) lives in [`AnchorKind`]; this is the
/// per-*kind* difference (what is read once we have it).
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
        content: ElementReader,
        max: usize,
    },
    /// A [`Watch::Record`]: walk `base` from the anchor to the record's base, then
    /// read each field relative to it into a [`Value::Map`].
    Record {
        base: Vec<i64>,
        fields: RecordFields,
    },
    /// A [`Watch::Derived`]: read **no memory at all** — fold the values other
    /// watches already produced this tick. The odd one out here, and deliberately
    /// so: it has no anchor to resolve, so it never reaches the anchor lookup, and
    /// it is sampled in its own phase of [`Session::poll`] once `last` holds this
    /// tick's readings.
    Derived {
        /// The earlier collection watch to evaluate once per element, if any.
        each: Option<String>,
        /// The expression, evaluated in `f64` and coerced to `ty`.
        expr: Expr,
        /// The declared output type.
        ty: ValueType,
    },
}

/// A watch reduced to what the loop needs each tick, plus its schedule state.
struct Scheduled {
    name: String,
    /// How this watch's anchor is (re)found — `None` for a [`Reader::Derived`],
    /// which has no anchor at all because it reads no memory. Modelled as an
    /// absence rather than a stand-in [`AnchorKind`]: there is no address to
    /// invent, and a fake one would have to be excluded from re-attach anyway.
    kind: Option<AnchorKind>,
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

/// Split a collection/record [`Base`] into how its anchor is *found*
/// ([`AnchorKind`]) and the offset chain from that anchor to the container or
/// record base. Shared by the [`Watch::Collection`] and [`Watch::Record`] attach
/// paths — a base is anchored exactly like a scalar watch.
fn anchor_from_base(base: &Base) -> (AnchorKind, Vec<i64>) {
    match base {
        Base::Tier1 { module, offsets } => (AnchorKind::Module(module.clone()), offsets.clone()),
        Base::Tier2 {
            anchor,
            rip,
            offsets,
        } => (signature_kind(anchor, *rip), offsets.clone()),
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

/// Read a record: for each field, resolve its chain **relative to the same
/// `base`** and read its value into an ordered [`Value::Map`]. Reading every
/// field off one base in one call is what makes the record a coherent atomic
/// sample. A field whose chain can't be resolved is a nested
/// [`Unavailable`](Value::Unavailable) in place — the record still forms.
fn read_record<B: MemoryBackend + ?Sized>(backend: &B, base: u64, fields: &RecordFields) -> Value {
    let mut map = BTreeMap::new();
    for (name, offsets, ty) in fields {
        let value = match backend.resolve(base, offsets) {
            Ok(addr) => read_typed(backend, addr, *ty),
            Err(_) => Value::Unavailable,
        };
        map.insert(name.clone(), value);
    }
    Value::Map(map)
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

/// What a [derived](crate::profile::Watch::Derived) expression is evaluated
/// against: this tick's readings, plus the current element when the watch
/// iterates an `each` collection.
///
/// Note what is *not* here: no backend, no anchor, no address. A derived
/// expression is structurally incapable of reading memory, which is what keeps
/// the tier engine-agnostic and its failure story trivial.
struct Ctx<'a> {
    last: &'a BTreeMap<String, Value>,
    element: Option<&'a Value>,
}

/// Which list fold is being evaluated. The four share everything but how they
/// accumulate, so they share one walk of the list.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FoldKind {
    Sum,
    Count,
    Min,
    Max,
}

/// 2^64. `u64::MAX as f64` rounds *up* to exactly this, so it is the
/// **exclusive** upper bound a `u64` coercion has to stay below; using the
/// rounded maximum would admit a value that cannot round-trip.
const U64_LIMIT: f64 = 18_446_744_073_709_551_616.0;

/// A reading as an `f64`, or `None` when it simply is not a number.
///
/// A string, a list, a record or an `Unavailable` where arithmetic needs a
/// number is a *shape* error, and shape errors propagate: they make the whole
/// expression unavailable rather than contributing a zero nobody read.
fn number_of(value: &Value) -> Option<f64> {
    match value {
        Value::I32(n) => Some(*n as f64),
        Value::U32(n) => Some(*n as f64),
        Value::F32(x) => Some(*x as f64),
        Value::U64(n) => Some(*n as f64),
        Value::Str(_) | Value::List(_) | Value::Map(_) | Value::Unavailable => None,
    }
}

/// Narrow a watch's value by an optional list `index` and an optional map
/// `field`, in that order. Indexing a scalar, keying a list, or an index out of
/// range is a shape error — `None`, never a clamped neighbour.
fn pick<'a>(value: &'a Value, index: Option<i64>, field: Option<&str>) -> Option<&'a Value> {
    let value = match index {
        Some(i) => match value {
            Value::List(items) => items.get(usize::try_from(i).ok()?)?,
            _ => return None,
        },
        None => value,
    };
    match field {
        Some(key) => match value {
            Value::Map(fields) => fields.get(key),
            _ => None,
        },
        None => Some(value),
    }
}

/// Evaluate an expression to a number, or `None` for "no honest answer this
/// tick". Every failure path — a missing watch, a wrong shape, an index out of
/// range, a zero divisor — funnels here, which is why the tier needs no error
/// type of its own.
fn eval(expr: &Expr, ctx: &Ctx<'_>) -> Option<f64> {
    match expr {
        Expr::Const { value } => Some(*value),
        Expr::Ref {
            watch,
            index,
            field,
        } => {
            let value = ctx.last.get(watch)?;
            number_of(pick(value, *index, field.as_deref())?)
        }
        Expr::Item { item } => match ctx.element? {
            Value::Map(fields) => number_of(fields.get(item)?),
            // An element that is not a record has no fields to name; under an
            // `each` over a scalar collection this is the honest answer.
            _ => None,
        },
        Expr::Add { add } => accumulate(add, ctx, 0.0, |a, b| a + b),
        Expr::Mul { mul } => accumulate(mul, ctx, 1.0, |a, b| a * b),
        Expr::Sub { sub } => {
            let (a, b) = two(sub, ctx)?;
            Some(a - b)
        }
        Expr::Div { div } => {
            let (a, b) = two(div, ctx)?;
            // An infinity is not a reading. Division by zero is unavailable.
            if b == 0.0 {
                return None;
            }
            Some(a / b)
        }
        Expr::Min { min } => extremum(min, ctx, FoldKind::Min),
        Expr::Max { max } => extremum(max, ctx, FoldKind::Max),
        Expr::Sum { sum } => eval_fold(sum, ctx, FoldKind::Sum),
        Expr::Count { count } => eval_fold(count, ctx, FoldKind::Count),
    }
}

/// Fold n-ary operands into `unit` with `op`; any unavailable operand sinks the
/// whole node.
fn accumulate(operands: &[Expr], ctx: &Ctx<'_>, unit: f64, op: fn(f64, f64) -> f64) -> Option<f64> {
    let mut acc = unit;
    for operand in operands {
        acc = op(acc, eval(operand, ctx)?);
    }
    Some(acc)
}

/// Evaluate exactly two operands. A validated profile guarantees the arity; a
/// hand-built one gets `None` rather than a silently dropped operand.
fn two(operands: &[Expr], ctx: &Ctx<'_>) -> Option<(f64, f64)> {
    match operands {
        [a, b] => Some((eval(a, ctx)?, eval(b, ctx)?)),
        _ => None,
    }
}

/// `min`/`max` in whichever shape the profile wrote: an array of operands, or a
/// fold over a list watch.
fn extremum(extremum: &Extremum, ctx: &Ctx<'_>, kind: FoldKind) -> Option<f64> {
    match extremum {
        Extremum::Nary(operands) => {
            let mut acc: Option<f64> = None;
            for operand in operands {
                let x = eval(operand, ctx)?;
                acc = Some(match acc {
                    Some(a) if kind == FoldKind::Min => a.min(x),
                    Some(a) => a.max(x),
                    None => x,
                });
            }
            acc
        }
        Extremum::Fold(fold) => eval_fold(fold, ctx, kind),
    }
}

/// Fold a list-valued watch: take a prefix, keep the elements every clause
/// accepts, and accumulate.
///
/// The empty cases differ on purpose. A `sum` of nothing is `0` — "nothing
/// matched" is a real answer — while a `min`/`max` of nothing is unavailable,
/// because there is no neutral element to return honestly. An element that
/// cannot be read *inside* the range is unavailable for every kind: skipping it
/// would quietly under-report.
fn eval_fold(fold: &Fold, ctx: &Ctx<'_>, kind: FoldKind) -> Option<f64> {
    let items = match ctx.last.get(&fold.watch)? {
        Value::List(items) => items,
        _ => return None,
    };
    let take = match &fold.take {
        Some(take) => {
            let n = eval(take, ctx)?;
            // A negative take is a broken formula, not an empty range.
            if !n.is_finite() || n < 0.0 {
                return None;
            }
            (n as usize).min(items.len())
        }
        None => items.len(),
    };

    let mut matched = 0u64;
    let mut total = 0.0;
    let mut best: Option<f64> = None;
    for item in &items[..take] {
        if !matches_clauses(item, &fold.clauses)? {
            continue;
        }
        matched += 1;
        // Counting asks how many elements match, never what they hold, so it
        // needs no `field` and survives an element it could not have read.
        if kind == FoldKind::Count {
            continue;
        }
        let value = match &fold.field {
            Some(key) => match item {
                Value::Map(fields) => fields.get(key)?,
                _ => return None,
            },
            None => item,
        };
        let x = number_of(value)?;
        total += x;
        best = Some(match best {
            Some(b) if kind == FoldKind::Min => b.min(x),
            Some(b) => b.max(x),
            None => x,
        });
    }

    match kind {
        FoldKind::Sum => Some(total),
        FoldKind::Count => Some(matched as f64),
        FoldKind::Min | FoldKind::Max => best,
    }
}

/// Test one element against a fold's `where` clauses — all of which must hold.
///
/// `None` means the question could not be answered: a field the element lacks,
/// or a comparison between text and a number. That makes the whole fold
/// unavailable rather than silently dropping the element, which is the
/// difference between a filter and a lie.
fn matches_clauses(item: &Value, clauses: &[Clause]) -> Option<bool> {
    let mut all = true;
    for clause in clauses {
        let value = match item {
            Value::Map(fields) => fields.get(&clause.field)?,
            _ => return None,
        };
        // Every clause is evaluated, not short-circuited: a clause naming a
        // field that isn't there is a broken profile, and it should surface even
        // when an earlier clause already excluded the element.
        if !compare(value, &clause.test)? {
            all = false;
        }
    }
    Some(all)
}

/// Apply one clause's comparison to a field's reading.
fn compare(value: &Value, test: &Compare) -> Option<bool> {
    match test {
        Compare::Eq(lit) => equals(value, lit),
        Compare::Ne(lit) => equals(value, lit).map(|eq| !eq),
        Compare::Lt(lit) => order(value, lit).map(Ordering::is_lt),
        Compare::Le(lit) => order(value, lit).map(Ordering::is_le),
        Compare::Gt(lit) => order(value, lit).map(Ordering::is_gt),
        Compare::Ge(lit) => order(value, lit).map(Ordering::is_ge),
    }
}

/// Identity against a literal: numbers numerically, strings textually. A number
/// tested against a string reading (or the reverse) is undecidable, not `false`.
fn equals(value: &Value, lit: &Literal) -> Option<bool> {
    match lit {
        Literal::Num(n) => Some(number_of(value)? == *n),
        Literal::Text(s) => match value {
            Value::Str(v) => Some(v == s),
            _ => None,
        },
    }
}

/// Ordering against a **numeric** literal. Text orders under nothing but
/// `eq`/`ne`, so a string operand is undecidable here rather than collated.
fn order(value: &Value, lit: &Literal) -> Option<Ordering> {
    match lit {
        Literal::Num(n) => number_of(value)?.partial_cmp(n),
        Literal::Text(_) => None,
    }
}

/// Coerce an evaluated `f64` to the watch's declared type.
///
/// Integer types truncate toward zero; anything the target cannot hold — a
/// non-finite result, or one outside its range — is `Unavailable`. Never a
/// saturated number: `i32::MAX` reported for an overflow would be a lie a
/// consumer has no way to detect.
fn coerce(value: Option<f64>, ty: ValueType) -> Value {
    let x = match value {
        Some(x) if x.is_finite() => x,
        _ => return Value::Unavailable,
    };
    match ty {
        // An f32 keeps its fraction; a magnitude it cannot hold becomes infinite,
        // which is the same "no honest reading" as any other overflow.
        ValueType::F32 => match x as f32 {
            f if f.is_finite() => Value::F32(f),
            _ => Value::Unavailable,
        },
        ValueType::I32 => match x.trunc() {
            n if (i32::MIN as f64..=i32::MAX as f64).contains(&n) => Value::I32(n as i32),
            _ => Value::Unavailable,
        },
        ValueType::U32 => match x.trunc() {
            n if (0.0..=u32::MAX as f64).contains(&n) => Value::U32(n as u32),
            _ => Value::Unavailable,
        },
        ValueType::U64 => match x.trunc() {
            n if (0.0..U64_LIMIT).contains(&n) => Value::U64(n as u64),
            _ => Value::Unavailable,
        },
        // Unreachable in a validated profile — a derived watch may not declare a
        // string type — and still not a guess if a hand-built one does.
        ValueType::String(_) => Value::Unavailable,
    }
}

/// Evaluate a derived watch against the readings already in `last`.
///
/// Without an `each` this is one scalar. With one, the named collection is
/// walked and the expression evaluated per element: an element whose expression
/// fails is a nested `Unavailable` in place and the list still forms — exactly
/// how a collection already treats a broken element. A missing, unavailable or
/// non-list `each` target makes the whole watch unavailable: there is nothing to
/// iterate, so there is no list to emit.
fn derive(last: &BTreeMap<String, Value>, each: Option<&str>, expr: &Expr, ty: ValueType) -> Value {
    let each = match each {
        Some(name) => name,
        // No `each`: a single scalar, evaluated with no current element.
        None => {
            let ctx = Ctx {
                last,
                element: None,
            };
            return coerce(eval(expr, &ctx), ty);
        }
    };
    let items = match last.get(each) {
        Some(Value::List(items)) => items,
        _ => return Value::Unavailable,
    };
    let values = items
        .iter()
        .map(|element| {
            let ctx = Ctx {
                last,
                element: Some(element),
            };
            coerce(eval(expr, &ctx), ty)
        })
        .collect();
    Value::List(values)
}

/// Sample one watch. A derived watch folds this tick's readings and returns
/// before any anchor is consulted — it has none. Otherwise a scalar walks its
/// chain and reads one value, a collection iterates its container, and every
/// failure path returns `Unavailable` (or, per element, a nested `Unavailable`)
/// — never a guess.
fn sample_one<B: MemoryBackend + ?Sized>(
    backend: &B,
    w: &Scheduled,
    last: &BTreeMap<String, Value>,
) -> Value {
    // Before the anchor lookup, because a derived watch has no anchor: it reads
    // no memory, only what the memory watches already produced.
    if let Reader::Derived { each, expr, ty } = &w.reader {
        return derive(last, each.as_deref(), expr, *ty);
    }
    let anchor = match w.anchor {
        Some(a) => a,
        None => return Value::Unavailable,
    };
    match &w.reader {
        Reader::Scalar { offsets, ty } => match backend.resolve(anchor, offsets) {
            Ok(addr) => read_typed(backend, addr, *ty),
            Err(_) => Value::Unavailable,
        },
        Reader::Record { base, fields } => match backend.resolve(anchor, base) {
            // Reach the record's base once, then read every field relative to it.
            // A base that no longer resolves makes the whole record unavailable;
            // a single broken field stays a nested Unavailable (in read_record).
            Ok(base_addr) => read_record(backend, base_addr, fields),
            Err(_) => Value::Unavailable,
        },
        Reader::Collection {
            base,
            count,
            items,
            first,
            stride,
            element,
            content,
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
                // Resolve the element's address once (shared by every field of a
                // record); a failure here fails the whole element, not one field.
                let value = match backend.resolve(slot, element) {
                    Ok(addr) => match content {
                        ElementReader::Scalar(ty) => read_typed(backend, addr, *ty),
                        ElementReader::Record(fields) => read_record(backend, addr, fields),
                    },
                    Err(_) => Value::Unavailable,
                };
                out.push(value);
            }
            Value::List(out)
        }
        // Answered above, before the anchor lookup this arm sits behind.
        Reader::Derived { .. } => Value::Unavailable,
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
                    Some(AnchorKind::Module(module.clone())),
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
                    Some(signature_kind(anchor, *rip)),
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
                    fields,
                    max,
                    rate_hz,
                } => {
                    // A collection's base is anchored exactly like a scalar
                    // watch; only its `offsets` reach the container rather than a
                    // value. Everything after the base is the iteration recipe.
                    let (kind, base_offsets) = anchor_from_base(base);
                    // Scalar-XOR-record element (validation guarantees exactly one
                    // in a parsed profile; the scalar fallback is only reached by a
                    // hand-built, unvalidated profile and never read wrongly).
                    let content = match fields {
                        Some(fields) => ElementReader::Record(flatten_fields(fields)),
                        None => ElementReader::Scalar(ty.unwrap_or(ValueType::U64)),
                    };
                    (
                        name.clone(),
                        Some(kind),
                        Reader::Collection {
                            base: base_offsets,
                            count: count.clone(),
                            items: items.clone(),
                            first: *first,
                            stride: *stride,
                            element: element.clone(),
                            content,
                            max: *max,
                        },
                        *rate_hz,
                    )
                }
                Watch::Record {
                    name,
                    base,
                    fields,
                    rate_hz,
                } => {
                    // A record's base is anchored exactly like a collection's; the
                    // fields are then read relative to the resolved base.
                    let (kind, base_offsets) = anchor_from_base(base);
                    (
                        name.clone(),
                        Some(kind),
                        Reader::Record {
                            base: base_offsets,
                            fields: flatten_fields(fields),
                        },
                        *rate_hz,
                    )
                }
                Watch::Derived {
                    name,
                    ty,
                    each,
                    value,
                    rate_hz,
                } => (
                    name.clone(),
                    // No anchor: this watch reads no memory, so there is nothing
                    // to resolve now and nothing to re-resolve on a re-attach.
                    None,
                    Reader::Derived {
                        each: each.clone(),
                        expr: value.clone(),
                        ty: *ty,
                    },
                    *rate_hz,
                ),
            };
            let anchor = kind.as_ref().and_then(|k| resolve_anchor(&backend, k));
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
    ///
    /// A tick runs in **two phases**, and they must not interleave. Phase one
    /// samples every due *memory* watch, writing this tick's readings into
    /// `last`. Phase two evaluates every due *derived* watch against that same
    /// `last` — so it sees fresh values for whatever was sampled just now and the
    /// most recent value for whatever was not due, which is exactly the "latest
    /// known" semantics a computed value wants. Interleaved, a derived watch
    /// would see this tick's reading or last tick's depending on declaration
    /// order, which is no semantics at all.
    pub fn poll(&mut self, elapsed: Duration) -> Snapshot {
        let mut diff = Snapshot::new();
        let (sampled, failed) = self.sample_phase(elapsed, false, &mut diff);
        // Phase two's counts are deliberately discarded. A derived watch reads no
        // memory, so letting it vote on the re-attach decision would make a
        // formula error look like a process that moved out from under us — and a
        // healthy derived watch would mask a target that really is gone.
        let _ = self.sample_phase(elapsed, true, &mut diff);

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

    /// Sample the watches of one phase that are due at `elapsed`, folding what
    /// changed into `diff` and returning `(sampled, failed)` for the caller's
    /// re-attach bookkeeping. `derived` selects the phase: `false` for the memory
    /// watches, `true` for the derived ones.
    fn sample_phase(
        &mut self,
        elapsed: Duration,
        derived: bool,
        diff: &mut Snapshot,
    ) -> (u32, u32) {
        let mut sampled = 0u32;
        let mut failed = 0u32;
        for w in &mut self.watches {
            if matches!(w.reader, Reader::Derived { .. }) != derived {
                continue;
            }
            if elapsed < w.next_due {
                continue;
            }
            sampled += 1;
            w.next_due = elapsed + w.period;

            let value = sample_one(&self.backend, w, &self.last);
            if value == Value::Unavailable {
                failed += 1;
            }

            // Emit only genuine changes. A first sighting (no prior value) always
            // counts as a change, including a first-seen `Unavailable`. The clone
            // lands only on an actual change — a quiet tick copies nothing.
            //
            // Writing back here also feeds the phase: a derived watch declared
            // after another sees the value this loop just stored, which is why
            // declaration order is a sufficient evaluation order.
            let changed = self.last.get(&w.name) != Some(&value);
            if changed {
                self.last.insert(w.name.clone(), value.clone());
                diff.insert(w.name.clone(), value);
            }
        }
        (sampled, failed)
    }

    /// Re-resolve every watch's anchor against the live target. Called when a
    /// run of fully-failed ticks suggests the process moved (relocated module,
    /// freed region) — the same resolution attach did, repeated. A derived watch
    /// has no anchor and is skipped: there is nothing about it that a moved
    /// process could invalidate.
    fn reattach(&mut self) {
        for w in &mut self.watches {
            if let Some(kind) = &w.kind {
                w.anchor = resolve_anchor(&self.backend, kind);
            }
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
            while !stop_thread.load(AtomicOrdering::Relaxed) {
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
        self.stop.store(true, AtomicOrdering::Relaxed);
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

    /// The wire form is a **compatibility surface**: a host that parses it is
    /// entitled to this exact shape, so pin it. Untagged — the reading looks like
    /// what it is, because the consumer already knows each watch's declared type.
    #[test]
    fn values_serialise_untagged() {
        let cases = [
            (Value::I32(-7), "-7"),
            (Value::U32(7), "7"),
            (Value::U64(u64::MAX), "18446744073709551615"),
            (Value::F32(1.5), "1.5"),
            (Value::Str("VALERE".into()), "\"VALERE\""),
            // The fail-soft state is `null`, not a missing key and not a zero: a
            // host must be able to tell "went dark" from "reads as 0".
            (Value::Unavailable, "null"),
        ];
        for (value, expected) in cases {
            assert_eq!(serde_json::to_string(&value).unwrap(), expected);
        }
    }

    /// A `NaN` read mid-write has no JSON form and collapses to `null` — the same
    /// as `Unavailable`. Both mean "no meaningful reading", so pin the collapse
    /// deliberately rather than discover it in a host one day.
    #[test]
    fn non_finite_floats_serialise_as_null() {
        assert_eq!(
            serde_json::to_string(&Value::F32(f32::NAN)).unwrap(),
            "null"
        );
        assert_eq!(
            serde_json::to_string(&Value::F32(f32::INFINITY)).unwrap(),
            "null"
        );
    }

    /// The one level of structure survives the wire, nested and in order: a party
    /// roster of records is an array of objects, with a broken element `null` in
    /// place rather than sinking the list.
    #[test]
    fn structure_survives_serialisation() {
        let party = Value::List(vec![
            Value::Map(BTreeMap::from([
                ("hp".to_string(), Value::I32(42)),
                ("name".to_string(), Value::Str("ZALE".into())),
            ])),
            Value::Unavailable,
        ]);
        assert_eq!(
            serde_json::to_string(&party).unwrap(),
            r#"[{"hp":42,"name":"ZALE"},null]"#
        );

        // And a whole snapshot is just a label -> reading object, key-ordered.
        let snapshot: Snapshot = BTreeMap::from([
            ("hp".to_string(), Value::I32(9)),
            ("party".to_string(), party),
        ]);
        assert_eq!(
            serde_json::to_string(&snapshot).unwrap(),
            r#"{"hp":9,"party":[{"hp":42,"name":"ZALE"},null]}"#
        );
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
            contract: None,
            contract_version: None,
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

    /// A torn float is usually a NaN, and IEEE says `NaN != NaN`. The diff must
    /// not take that literally, or the value — and any list holding it — would
    /// be resent on every tick for as long as the game leaves it torn.
    #[test]
    fn a_nan_reading_is_reported_once_not_every_tick() {
        let fake = Rc::new(Fake::new(0x600));
        let nan_bits = f32::NAN.to_bits() as i32;
        fake.write_i32(0, nan_bits);
        // A collection of floats whose middle element is a NaN.
        plant_collection(&fake, &[0, nan_bits, 0]);
        let mut floats = i32_collection_watch("speeds", 8);
        if let Watch::Collection { ty, .. } = &mut floats {
            *ty = Some(ValueType::F32);
        }
        let profile = Profile {
            label: None,
            contract: None,
            contract_version: None,
            match_: ident(),
            watches: vec![
                Watch::Tier1 {
                    name: "speed".to_string(),
                    module: "fake".to_string(),
                    offsets: vec![0],
                    ty: ValueType::F32,
                    rate_hz: None,
                },
                floats,
            ],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());

        let first = s.poll(Duration::ZERO);
        assert!(matches!(first.get("speed"), Some(Value::F32(x)) if x.is_nan()));
        assert!(first.contains_key("speeds"));
        assert!(
            s.poll(Duration::from_millis(50)).is_empty(),
            "an unchanged NaN must not count as a change"
        );

        // A NaN with a different payload is still "no reading", so still quiet.
        fake.write_i32(0, (f32::NAN.to_bits() | 1) as i32);
        assert!(s.poll(Duration::from_millis(100)).is_empty());

        // A real value arriving is a change, as ever.
        fake.write_i32(0, 1.5f32.to_bits() as i32);
        assert_eq!(
            s.poll(Duration::from_millis(150)).get("speed"),
            Some(&Value::F32(1.5))
        );
    }

    #[test]
    fn per_watch_rate_throttles_sampling() {
        let fake = Rc::new(Fake::new(64));
        let profile = Profile {
            label: None,
            contract: None,
            contract_version: None,
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
            contract: None,
            contract_version: None,
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
            contract: None,
            contract_version: None,
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
            contract: None,
            contract_version: None,
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
            contract: None,
            contract_version: None,
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
            contract: None,
            contract_version: None,
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
            ty: Some(ValueType::I32),
            fields: None,
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
            contract: None,
            contract_version: None,
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
            contract: None,
            contract_version: None,
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
            contract: None,
            contract_version: None,
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
            contract: None,
            contract_version: None,
            match_: ident(),
            watches: vec![i32_collection_watch("enemy_hp", 64)],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());
        assert_eq!(
            s.poll(Duration::ZERO).get("enemy_hp"),
            Some(&Value::Unavailable)
        );
    }

    /// Assemble a `Value::Map` from `(name, value)` pairs — the expected shape for
    /// a record, keyed and ordered like the engine emits.
    fn map(pairs: &[(&str, Value)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    }

    /// A record's `fields`, built from `(name, offsets, ty)` triples.
    fn fields(specs: &[(&str, Vec<i64>, ValueType)]) -> BTreeMap<String, Field> {
        specs
            .iter()
            .map(|(name, offsets, ty)| {
                (
                    name.to_string(),
                    Field {
                        offsets: offsets.clone(),
                        ty: *ty,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn record_watch_reads_a_map_off_a_shared_base() {
        // A top-level record: resolve one base, read two fields relative to it.
        // The fields land in the same tick off the same base — the atomic sample.
        let fake = Rc::new(Fake::new(0x100));
        fake.write_i32(0x40, 120); // hp
        fake.write_i32(0x44, 30); // sp

        let profile = Profile {
            label: None,
            contract: None,
            contract_version: None,
            match_: ident(),
            watches: vec![Watch::Record {
                name: "player".to_string(),
                base: Base::Tier1 {
                    module: "fake".to_string(),
                    offsets: vec![0x40], // module base + 0x40 = the record base
                },
                fields: fields(&[
                    ("hp", vec![0], ValueType::I32),
                    ("sp", vec![4], ValueType::I32),
                ]),
                rate_hz: None,
            }],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());
        assert_eq!(
            s.poll(Duration::ZERO).get("player"),
            Some(&map(&[("hp", Value::I32(120)), ("sp", Value::I32(30))])),
            "a record must emit a Value::Map of its named fields"
        );
    }

    #[test]
    fn collection_of_records_reads_a_map_per_element() {
        // `party = [{hp, mp}, …]`: each element is a record read off the member
        // object the slot points at. `element = [0, 0]` factors the shared deref;
        // each field is a short chain relative to that member base.
        let fake = Rc::new(Fake::new(0x400));
        let base = fake.base;
        fake.write_u64(0x100, base + 0x200); // items -> backing array
        fake.write_i32(0x108, 2); // count
        fake.write_u64(0x220, base + 0x300); // slot0 -> member0
        fake.write_i32(0x300, 100); // member0.hp
        fake.write_i32(0x304, 20); // member0.mp
        fake.write_u64(0x228, base + 0x340); // slot1 -> member1
        fake.write_i32(0x340, 80); // member1.hp
        fake.write_i32(0x344, 55); // member1.mp

        let profile = Profile {
            label: None,
            contract: None,
            contract_version: None,
            match_: ident(),
            watches: vec![Watch::Collection {
                name: "party".to_string(),
                base: Base::Tier1 {
                    module: "fake".to_string(),
                    offsets: vec![0x100],
                },
                count: vec![0x8],
                items: Some(vec![0x0]),
                first: 0x20,
                stride: 8,
                element: vec![0, 0], // slot -> deref -> member base
                ty: None,
                fields: Some(fields(&[
                    ("hp", vec![0], ValueType::I32),
                    ("mp", vec![4], ValueType::I32),
                ])),
                max: 8,
                rate_hz: None,
            }],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());
        assert_eq!(
            s.poll(Duration::ZERO).get("party"),
            Some(&Value::List(vec![
                map(&[("hp", Value::I32(100)), ("mp", Value::I32(20))]),
                map(&[("hp", Value::I32(80)), ("mp", Value::I32(55))]),
            ])),
            "a record collection must emit an ordered list of coherent maps"
        );
    }

    #[test]
    fn record_field_fails_soft_without_sinking_the_record() {
        // One field walks a bad pointer; it must be a nested Unavailable in place
        // while the healthy field stays live — the record still forms.
        let fake = Rc::new(Fake::new(0x100));
        fake.write_i32(0x40, 7); // hp (good)
        fake.write_u64(0x48, 0xdead_0000); // a pointer into unmapped memory

        let profile = Profile {
            label: None,
            contract: None,
            contract_version: None,
            match_: ident(),
            watches: vec![Watch::Record {
                name: "player".to_string(),
                base: Base::Tier1 {
                    module: "fake".to_string(),
                    offsets: vec![0x40],
                },
                fields: fields(&[
                    ("hp", vec![0], ValueType::I32),
                    ("bad", vec![0x8, 0], ValueType::I32), // deref 0xdead_0000 -> fails
                ]),
                rate_hz: None,
            }],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());
        assert_eq!(
            s.poll(Duration::ZERO).get("player"),
            Some(&map(&[("bad", Value::Unavailable), ("hp", Value::I32(7)),])),
            "a broken field is Unavailable in place, never sinking the record"
        );
    }

    #[test]
    fn record_base_unresolvable_is_wholly_unavailable() {
        // If the record's base can't be reached there is nothing to read fields
        // off — the whole record is Unavailable, not a map of Unavailables.
        let fake = Rc::new(Fake::new(0x40));
        let profile = Profile {
            label: None,
            contract: None,
            contract_version: None,
            match_: ident(),
            watches: vec![Watch::Record {
                name: "player".to_string(),
                base: Base::Tier1 {
                    module: "fake".to_string(),
                    offsets: vec![0x100, 0], // deref past the mapped region -> fails
                },
                fields: fields(&[("hp", vec![0], ValueType::I32)]),
                rate_hz: None,
            }],
        };
        let mut s = Session::attach(Rc::clone(&fake), &profile, Config::default());
        assert_eq!(
            s.poll(Duration::ZERO).get("player"),
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
            contract: None,
            contract_version: None,
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
            contract: None,
            contract_version: None,
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
            contract: None,
            contract_version: None,
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

    // ---- derived watches ---------------------------------------------------

    /// Assemble a profile around `watches` — the derived tests declare several
    /// at a time, and only the watch list ever differs.
    fn profile(watches: Vec<Watch>) -> Profile {
        Profile {
            label: None,
            contract: None,
            contract_version: None,
            match_: ident(),
            watches,
        }
    }

    /// A derived watch whose expression is given as the JSON an author would
    /// actually type. Building it this way keeps the tests readable *and* pins
    /// the untagged node discrimination the docs promise — an expression that
    /// parses wrongly here fails as loudly as one that evaluates wrongly.
    fn derived(name: &str, ty: ValueType, each: Option<&str>, value: &str) -> Watch {
        Watch::Derived {
            name: name.to_string(),
            ty,
            each: each.map(str::to_string),
            value: serde_json::from_str(value).expect("parse expression"),
            rate_hz: None,
        }
    }

    /// A Tier-1 watch whose chain derefs past the mapped region — permanently
    /// unreadable, so it stands in for any input that has gone dark.
    fn broken_tier1(name: &str) -> Watch {
        Watch::Tier1 {
            name: name.to_string(),
            module: "fake".to_string(),
            offsets: vec![0x1000, 0],
            ty: ValueType::I32,
            rate_hz: None,
        }
    }

    #[test]
    fn derived_watch_folds_other_watches_and_diffs_like_any_value() {
        let fake = Rc::new(Fake::new(64));
        fake.write_i32(0, 100);
        fake.write_i32(8, 25);
        let p = profile(vec![
            tier1("hp", 0, None),
            tier1("shield", 8, None),
            derived(
                "effective_hp",
                ValueType::I32,
                None,
                r#"{ "add": [{ "watch": "hp" }, { "watch": "shield" }] }"#,
            ),
        ]);
        let mut s = Session::attach(Rc::clone(&fake), &p, Config::default());

        assert_eq!(
            s.poll(Duration::ZERO).get("effective_hp"),
            Some(&Value::I32(125))
        );
        // Quiet inputs, quiet output: a derived watch diffs like any other value.
        assert!(s.poll(Duration::from_millis(50)).is_empty());

        // Move an input: the derived value must reflect the reading taken in the
        // *same* tick, which is the whole reason poll runs in two phases.
        fake.write_i32(0, 90);
        let d = s.poll(Duration::from_millis(100));
        assert_eq!(d.get("hp"), Some(&Value::I32(90)));
        assert_eq!(
            d.get("effective_hp"),
            Some(&Value::I32(115)),
            "a derived watch must fold this tick's readings, not last tick's"
        );
    }

    #[test]
    fn an_unavailable_input_makes_the_whole_expression_unavailable() {
        let fake = Rc::new(Fake::new(64));
        fake.write_i32(0, 10);
        let p = profile(vec![
            tier1("hp", 0, None),
            broken_tier1("gone"),
            derived(
                "total",
                ValueType::I32,
                None,
                r#"{ "add": [{ "watch": "hp" }, { "watch": "gone" }] }"#,
            ),
        ]);
        let mut s = Session::attach(Rc::clone(&fake), &p, Config::default());
        let d = s.poll(Duration::ZERO);
        assert_eq!(d.get("gone"), Some(&Value::Unavailable));
        assert_eq!(
            d.get("total"),
            Some(&Value::Unavailable),
            "one unavailable input sinks the whole expression — never a partial sum"
        );
    }

    #[test]
    fn division_by_zero_and_an_out_of_range_result_are_unavailable() {
        let fake = Rc::new(Fake::new(64));
        fake.write_i32(0, 10);
        fake.write_i32(4, 0);
        fake.write_i32(8, i32::MAX);
        let p = profile(vec![
            tier1("hp", 0, None),
            tier1("zero", 4, None),
            tier1("huge", 8, None),
            derived(
                "ratio",
                ValueType::I32,
                None,
                r#"{ "div": [{ "watch": "hp" }, { "watch": "zero" }] }"#,
            ),
            derived(
                "overflow",
                ValueType::I32,
                None,
                r#"{ "mul": [{ "watch": "huge" }, { "const": 2 }] }"#,
            ),
            derived(
                "quarter",
                ValueType::I32,
                None,
                r#"{ "div": [{ "watch": "hp" }, { "const": 4 }] }"#,
            ),
        ]);
        let mut s = Session::attach(Rc::clone(&fake), &p, Config::default());
        let d = s.poll(Duration::ZERO);
        assert_eq!(
            d.get("ratio"),
            Some(&Value::Unavailable),
            "division by zero is unavailable, never an infinity"
        );
        assert_eq!(
            d.get("overflow"),
            Some(&Value::Unavailable),
            "an out-of-range result is unavailable, never a saturated number"
        );
        assert_eq!(
            d.get("quarter"),
            Some(&Value::I32(2)),
            "an in-range fraction truncates toward zero"
        );
    }

    #[test]
    fn sum_takes_a_prefix_and_clamps_it_to_the_list() {
        let fake = Rc::new(Fake::new(0x600));
        plant_collection(&fake, &[11, 22, 33]);
        let p = profile(vec![
            i32_collection_watch("enemy_hp", 64),
            derived(
                "first_two",
                ValueType::I32,
                None,
                r#"{ "sum": { "watch": "enemy_hp", "take": { "const": 2 } } }"#,
            ),
            derived(
                "past_the_end",
                ValueType::I32,
                None,
                r#"{ "sum": { "watch": "enemy_hp", "take": { "const": 9 } } }"#,
            ),
            derived(
                "none_of_them",
                ValueType::I32,
                None,
                r#"{ "sum": { "watch": "enemy_hp", "take": { "const": 0 } } }"#,
            ),
            derived(
                "negative_take",
                ValueType::I32,
                None,
                r#"{ "sum": { "watch": "enemy_hp", "take": { "const": -1 } } }"#,
            ),
        ]);
        let mut s = Session::attach(Rc::clone(&fake), &p, Config::default());
        let d = s.poll(Duration::ZERO);
        assert_eq!(d.get("first_two"), Some(&Value::I32(33)));
        assert_eq!(
            d.get("past_the_end"),
            Some(&Value::I32(66)),
            "a take past the end clamps to the list rather than failing"
        );
        assert_eq!(d.get("none_of_them"), Some(&Value::I32(0)));
        assert_eq!(
            d.get("negative_take"),
            Some(&Value::Unavailable),
            "a negative take is a broken formula, not an empty range"
        );
    }

    /// Plant a **polymorphic** modifier list: elements of different kinds in one
    /// array, each `{kind: string, stat: i32, amount: i32}`. This is the shape a
    /// `where` clause exists for — without a clause on the type tag, an entry of
    /// the wrong kind contributes a number that means nothing.
    fn plant_modifiers(fake: &Fake, mods: &[(&str, i32, i32)]) {
        let base = fake.base;
        fake.write_u64(0x100, base + 0x200); // items -> backing array
        fake.write_i32(0x108, mods.len() as i32); // count
        for (i, (kind, stat, amount)) in mods.iter().enumerate() {
            let elem = 0x300 + i * 0x40;
            let text = 0x400 + i * 0x80;
            fake.write_u64(0x220 + i * 8, base + elem as u64); // slot -> element
            fake.write_u64(elem, base + text as u64); // element.kind -> string
            fake.write_i32(elem + 8, *stat);
            fake.write_i32(elem + 12, *amount);
            let utf16: Vec<u8> = kind.encode_utf16().flat_map(u16::to_le_bytes).collect();
            fake.write_i32(text + 0x10, (utf16.len() / 2) as i32);
            fake.write_bytes(text + 0x14, &utf16);
        }
    }

    /// The record collection over [`plant_modifiers`]'s layout.
    fn modifier_collection(name: &str) -> Watch {
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
            element: vec![0, 0],
            ty: None,
            fields: Some(fields(&[
                ("kind", vec![0], il2cpp_string()),
                ("stat", vec![8], ValueType::I32),
                ("amount", vec![12], ValueType::I32),
            ])),
            max: 16,
            rate_hz: None,
        }
    }

    #[test]
    fn a_multi_clause_where_filters_a_polymorphic_list() {
        let fake = Rc::new(Fake::new(0x800));
        // The third entry is of another kind entirely: its `stat` slot is not a
        // stat id and its `amount` is not an amount. A sum that trusts it lies.
        plant_modifiers(
            &fake,
            &[
                ("PlayerAddStatModifier", 0, 15),
                ("PlayerAddStatModifier", 1, 3),
                ("PercentageBasicAttackDamageHealModifier", 0, 999),
                ("PlayerAddStatModifier", 0, 10),
            ],
        );
        let p = profile(vec![
            modifier_collection("mods"),
            derived(
                "hp_bonus",
                ValueType::I32,
                None,
                r#"{ "sum": { "watch": "mods", "field": "amount", "where": [
                     { "field": "kind", "eq": "PlayerAddStatModifier" },
                     { "field": "stat", "eq": 0 } ] } }"#,
            ),
            derived(
                "unfiltered",
                ValueType::I32,
                None,
                r#"{ "sum": { "watch": "mods", "field": "amount",
                     "where": [{ "field": "stat", "eq": 0 }] } }"#,
            ),
            derived(
                "add_stat_mods",
                ValueType::U32,
                None,
                r#"{ "count": { "watch": "mods",
                     "where": [{ "field": "kind", "eq": "PlayerAddStatModifier" }] } }"#,
            ),
        ]);
        let mut s = Session::attach(Rc::clone(&fake), &p, Config::default());
        let d = s.poll(Duration::ZERO);
        assert_eq!(
            d.get("hp_bonus"),
            Some(&Value::I32(25)),
            "both clauses must hold: 15 + 10, and not the foreign entry"
        );
        assert_eq!(
            d.get("unfiltered"),
            Some(&Value::I32(1024)),
            "without the type-tag clause the foreign entry's bytes join the sum — \
             the exact silent garbage the clause exists to exclude"
        );
        assert_eq!(
            d.get("add_stat_mods"),
            Some(&Value::U32(3)),
            "count reports how many elements matched, needing no field"
        );
    }

    #[test]
    fn a_where_clause_naming_a_missing_field_is_unavailable() {
        let fake = Rc::new(Fake::new(0x800));
        plant_modifiers(&fake, &[("PlayerAddStatModifier", 0, 15)]);
        let p = profile(vec![
            modifier_collection("mods"),
            derived(
                "typo",
                ValueType::I32,
                None,
                r#"{ "sum": { "watch": "mods", "field": "amount",
                     "where": [{ "field": "staat", "eq": 0 }] } }"#,
            ),
        ]);
        let mut s = Session::attach(Rc::clone(&fake), &p, Config::default());
        assert_eq!(
            s.poll(Duration::ZERO).get("typo"),
            Some(&Value::Unavailable),
            "a clause that cannot be answered must not quietly filter everything out"
        );
    }

    #[test]
    fn an_empty_sum_is_zero_but_an_empty_extremum_is_unavailable() {
        let fake = Rc::new(Fake::new(0x600));
        plant_collection(&fake, &[]);
        let p = profile(vec![
            i32_collection_watch("enemy_hp", 64),
            derived(
                "total",
                ValueType::I32,
                None,
                r#"{ "sum": { "watch": "enemy_hp" } }"#,
            ),
            derived(
                "worst",
                ValueType::I32,
                None,
                r#"{ "min": { "watch": "enemy_hp" } }"#,
            ),
        ]);
        let mut s = Session::attach(Rc::clone(&fake), &p, Config::default());
        let d = s.poll(Duration::ZERO);
        assert_eq!(
            d.get("enemy_hp"),
            Some(&Value::List(vec![])),
            "the fixture really is an empty list, not a failed read"
        );
        assert_eq!(
            d.get("total"),
            Some(&Value::I32(0)),
            "nothing matched is a real answer for a sum"
        );
        assert_eq!(
            d.get("worst"),
            Some(&Value::Unavailable),
            "an extremum over nothing has no neutral element to report honestly"
        );
    }

    #[test]
    fn a_sum_spanning_a_broken_element_is_unavailable() {
        let fake = Rc::new(Fake::new(0x600));
        plant_collection(&fake, &[11, 22, 33]);
        // The middle slot points at unmapped memory: the list still forms, with a
        // nested Unavailable — but a sum across it would under-report by 22.
        fake.write_u64(0x228, 0xdead_0000);
        let p = profile(vec![
            i32_collection_watch("enemy_hp", 64),
            derived(
                "total",
                ValueType::I32,
                None,
                r#"{ "sum": { "watch": "enemy_hp" } }"#,
            ),
            derived(
                "first_only",
                ValueType::I32,
                None,
                r#"{ "sum": { "watch": "enemy_hp", "take": { "const": 1 } } }"#,
            ),
        ]);
        let mut s = Session::attach(Rc::clone(&fake), &p, Config::default());
        let d = s.poll(Duration::ZERO);
        assert_eq!(
            d.get("total"),
            Some(&Value::Unavailable),
            "skipping an unreadable element would quietly under-report the sum"
        );
        assert_eq!(
            d.get("first_only"),
            Some(&Value::I32(11)),
            "…but a range that stops short of it is perfectly answerable"
        );
    }

    #[test]
    fn min_and_max_accept_both_the_n_ary_and_the_fold_form() {
        let fake = Rc::new(Fake::new(0x600));
        plant_collection(&fake, &[11, 22, 33]);
        fake.write_i32(0x10, -5); // a stat that has gone negative
        let p = profile(vec![
            i32_collection_watch("enemy_hp", 64),
            tier1("drain", 0x10, None),
            derived(
                "clamped",
                ValueType::I32,
                None,
                r#"{ "max": [{ "const": 0 }, { "watch": "drain" }] }"#,
            ),
            derived(
                "worst_enemy",
                ValueType::I32,
                None,
                r#"{ "min": { "watch": "enemy_hp" } }"#,
            ),
            derived(
                "best_enemy",
                ValueType::I32,
                None,
                r#"{ "max": { "watch": "enemy_hp" } }"#,
            ),
        ]);
        let mut s = Session::attach(Rc::clone(&fake), &p, Config::default());
        let d = s.poll(Duration::ZERO);
        assert_eq!(
            d.get("clamped"),
            Some(&Value::I32(0)),
            "the array form is n-ary over operands — here a clamp at zero"
        );
        assert_eq!(d.get("worst_enemy"), Some(&Value::I32(11)));
        assert_eq!(
            d.get("best_enemy"),
            Some(&Value::I32(33)),
            "the object form folds a list; the two never collide"
        );
    }

    #[test]
    fn each_emits_a_list_with_a_failing_element_unavailable_in_place() {
        let fake = Rc::new(Fake::new(0x400));
        let base = fake.base;
        fake.write_u64(0x100, base + 0x200); // items -> backing array
        fake.write_i32(0x108, 3); // count
        fake.write_u64(0x220, base + 0x300); // slot0 -> member0
        fake.write_i32(0x300, 100); // member0.hp
        fake.write_i32(0x304, 20); // member0.mp
        fake.write_u64(0x228, 0xdead_0000); // slot1 -> unmapped
        fake.write_u64(0x230, base + 0x340); // slot2 -> member2
        fake.write_i32(0x340, 80); // member2.hp
        fake.write_i32(0x344, 55); // member2.mp

        let p = profile(vec![
            Watch::Collection {
                name: "party".to_string(),
                base: Base::Tier1 {
                    module: "fake".to_string(),
                    offsets: vec![0x100],
                },
                count: vec![0x8],
                items: Some(vec![0x0]),
                first: 0x20,
                stride: 8,
                element: vec![0, 0],
                ty: None,
                fields: Some(fields(&[
                    ("hp", vec![0], ValueType::I32),
                    ("mp", vec![4], ValueType::I32),
                ])),
                max: 8,
                rate_hz: None,
            },
            derived(
                "reserves",
                ValueType::I32,
                Some("party"),
                r#"{ "add": [{ "item": "hp" }, { "item": "mp" }] }"#,
            ),
        ]);
        let mut s = Session::attach(Rc::clone(&fake), &p, Config::default());
        assert_eq!(
            s.poll(Duration::ZERO).get("reserves"),
            Some(&Value::List(vec![
                Value::I32(120),
                Value::Unavailable,
                Value::I32(135),
            ])),
            "an element whose expression fails is unavailable in place; the list forms"
        );
    }

    #[test]
    fn an_each_over_a_watch_that_is_not_a_list_is_wholly_unavailable() {
        // Too small a region for the container chain to resolve at all: the
        // collection is Unavailable rather than a list, so `each` has nothing to
        // iterate.
        let fake = Rc::new(Fake::new(0x10));
        let p = profile(vec![
            i32_collection_watch("enemy_hp", 64),
            derived(
                "doubled",
                ValueType::I32,
                Some("enemy_hp"),
                r#"{ "mul": [{ "item": "hp" }, { "const": 2 }] }"#,
            ),
        ]);
        let mut s = Session::attach(Rc::clone(&fake), &p, Config::default());
        let d = s.poll(Duration::ZERO);
        assert_eq!(d.get("enemy_hp"), Some(&Value::Unavailable));
        assert_eq!(
            d.get("doubled"),
            Some(&Value::Unavailable),
            "no list to walk means no list to emit — not an empty one"
        );
    }

    #[test]
    fn derived_watches_do_not_vote_on_the_reattach_decision() {
        // A derived watch reads no memory, so a healthy one must not mask a
        // target that has genuinely gone. Here `always` succeeds every tick (it
        // folds nothing but a literal) while the only memory watch fails: the
        // re-attach must still fire on schedule.
        let fake = Rc::new(Fake::new(64));
        fake.write_i32(0, 1);
        let p = profile(vec![
            tier1("hp", 0, None),
            derived("always", ValueType::I32, None, r#"{ "const": 1 }"#),
        ]);
        let config = Config {
            base_tick: Duration::from_millis(50),
            reattach_after: 3,
        };
        let mut s = Session::attach(Rc::clone(&fake), &p, config);
        assert_eq!(fake.base_calls.get(), 1, "attach resolved the base once");

        s.poll(Duration::ZERO);
        fake.fail.set(true);
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
            "a derived watch that still evaluates must not look like a live target"
        );
    }

    #[test]
    fn malformed_signature_watch_is_permanently_unavailable() {
        let fake = Rc::new(Fake::new(64));
        let profile = Profile {
            label: None,
            contract: None,
            contract_version: None,
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
