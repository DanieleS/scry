//! The **profile**: a data-driven description of *what* to read from a game and
//! *how to recognise the process* it belongs to.
//!
//! A profile lives in its own JSON file (community repo, independent update
//! cadence) — the crate never hard-codes a game. It carries two things:
//!
//! - a [`Match`] block, the identity logic the [resolver](crate::resolver) uses
//!   to decide whether this profile fits a running process, and
//! - a list of [`Watch`]es, the actual values to read once a profile is chosen.
//!
//! The filename is just a label; identity lives entirely in the `match` block —
//! above all in its `probe`, an AOB signature that must *actually resolve* in
//! the target for the profile to claim it. That is what makes same-engine
//! collisions self-resolving: a profile has to fit the memory, not merely share
//! an executable name.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Accept an integer offset written either as a JSON number (decimal) or as a
/// string — with an optional sign and a `0x`/`0X` prefix for hex, the notation a
/// disassembler and every memory tool actually speak. Values are stored as
/// `i64`; input is where the flexibility matters, so a profile authored from a
/// Cheat Engine session can paste `"0x58"` verbatim instead of hand-converting
/// it to `88`. (Serialization stays canonical decimal.)
pub(crate) mod hexnum {
    use serde::de::{self, Deserializer};
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Num(i64),
        Text(String),
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum FloatRepr {
        Num(f64),
        Text(String),
    }

    /// Parse a signed decimal or `0x`-prefixed hex integer: at most one sign,
    /// in front, then bare digits. Returns `None` on anything else, which the
    /// callers turn into a serde error.
    ///
    /// The digits are checked by hand because the standard parsers accept a
    /// sign of their own: left to them, `"0x-5"` read as `-5` and `"--5"` as
    /// `5`, neither of which is a number anyone meant to write.
    pub(crate) fn parse(text: &str) -> Option<i64> {
        let s = text.trim();
        let (negative, body) = match s.as_bytes().first() {
            Some(b'-') => (true, s[1..].trim_start()),
            Some(b'+') => (false, s[1..].trim_start()),
            _ => (false, s),
        };
        let (digits, radix) = match body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
            Some(hex) => (hex, 16),
            None => (body, 10),
        };
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return None;
        }
        // Parsed unsigned and negated in a wider type, so the full `i64` range
        // (down to `-0x8000000000000000`) is reachable and nothing overflows.
        let magnitude = i128::from(u64::from_str_radix(digits, radix).ok()?);
        i64::try_from(if negative { -magnitude } else { magnitude }).ok()
    }

    fn one<E: de::Error>(repr: Repr) -> Result<i64, E> {
        match repr {
            Repr::Num(n) => Ok(n),
            Repr::Text(s) => parse(&s)
                .ok_or_else(|| E::custom(format!("not a decimal or 0x-hex integer: {s:?}"))),
        }
    }

    /// `deserialize_with` for a single `i64` field (e.g. a `rip` displacement).
    pub fn de_i64<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
        one(Repr::deserialize(d)?)
    }

    /// `deserialize_with` for a `Vec<i64>` field (an offset chain), applying the
    /// number-or-hex-string rule to each element independently.
    pub fn de_vec_i64<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<i64>, D::Error> {
        Vec::<Repr>::deserialize(d)?.into_iter().map(one).collect()
    }

    /// `deserialize_with` for an *optional* `Vec<i64>` field, so a collection's
    /// `items` chain accepts hex-or-decimal like every other chain while still
    /// being omittable. Absent (or `null`) stays `None`.
    pub fn de_opt_vec_i64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<i64>>, D::Error> {
        match Option::<Vec<Repr>>::deserialize(d)? {
            None => Ok(None),
            Some(v) => v
                .into_iter()
                .map(one)
                .collect::<Result<Vec<_>, _>>()
                .map(Some),
        }
    }

    /// `deserialize_with` for an *optional* single `i64` (e.g. a string's
    /// `len_at`), accepting the same number-or-hex-string forms.
    pub fn de_opt_i64<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
        match Option::<Repr>::deserialize(d)? {
            None => Ok(None),
            Some(r) => one(r).map(Some),
        }
    }

    /// `deserialize_with` for an `f64` that accepts the same number-or-hex-string
    /// forms — a [derived](crate::profile::Expr) watch's `{"const": …}`.
    ///
    /// The literal is usually an integer, and often nicer read as the hex a
    /// disassembler showed, which is exactly what the helpers above are for. But
    /// the expression language evaluates in `f64`, so a fractional literal is a
    /// legitimate thing to write and is accepted rather than rejected on a
    /// technicality.
    pub fn de_f64<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        match FloatRepr::deserialize(d)? {
            FloatRepr::Num(x) => Ok(x),
            FloatRepr::Text(s) => parse(&s)
                .map(|n| n as f64)
                .ok_or_else(|| de::Error::custom(format!("not a decimal or 0x-hex number: {s:?}"))),
        }
    }
}

/// `skip_serializing_if` helper: a zero offset is the default and stays implicit.
fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// `skip_serializing_if` helper: `false` is the default and stays implicit.
fn is_false(b: &bool) -> bool {
    !*b
}

/// The type of a value read from memory. Each variant maps to one of the typed
/// reads on [`MemoryBackend`](crate::MemoryBackend).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ValueType {
    I32,
    U32,
    F32,
    U64,
    /// A text string. Unlike the numeric types, a string's bytes are not a fixed
    /// field: its length, encoding, char offset, and whether it lives behind a
    /// pointer all vary **by engine** (IL2CPP, Mono, native C, Unreal `FString`,
    /// …). So the layout is *data*, carried in a [`StringSpec`] — a named preset
    /// or an explicit descriptor — rather than baked into the runtime. No engine
    /// is the default: `"i32"` stays a bare tag, but a string must name its shape
    /// (`{"string": "il2cpp"}` or `{"string": { … }}`).
    String(StringSpec),
}

impl ValueType {
    /// The concrete [`StringLayout`] for a [`String`](ValueType::String) type
    /// (expanding a preset), or `None` for the numeric types.
    pub fn string_layout(self) -> Option<StringLayout> {
        match self {
            ValueType::String(spec) => Some(spec.layout()),
            _ => None,
        }
    }
}

/// How to read a [string](ValueType::String): a named [preset](StringPreset) for
/// a known engine's layout, or an explicit [layout](StringLayout). Serialized
/// untagged, so `"il2cpp"` and `{ "encoding": …, … }` are both accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StringSpec {
    /// A named engine preset, expanded to a [`StringLayout`] at attach time.
    Preset(StringPreset),
    /// An explicit layout — the escape hatch for any engine without a preset.
    Layout(StringLayout),
}

impl StringSpec {
    /// Resolve to the concrete [`StringLayout`] the engine reads with, expanding
    /// a preset to its known offsets.
    pub fn layout(self) -> StringLayout {
        match self {
            StringSpec::Preset(p) => p.layout(),
            StringSpec::Layout(l) => l,
        }
    }
}

/// A named string layout for an engine whose shape we've validated. IL2CPP is
/// the first and (today) only one — a *peer* entry here, not a privileged
/// default. New engines earn a preset once validated against a real target;
/// until then they use an explicit [`StringLayout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StringPreset {
    /// IL2CPP `System.String`: a reference to an object holding a 32-bit UTF-16
    /// code-unit count at `+0x10` and the payload at `+0x14`.
    Il2cpp,
}

impl StringPreset {
    /// The concrete layout this preset stands for.
    pub fn layout(self) -> StringLayout {
        match self {
            StringPreset::Il2cpp => StringLayout {
                encoding: StringEncoding::Utf16,
                len_at: Some(0x10),
                chars_at: 0x14,
                deref: true,
            },
        }
    }
}

/// Text encoding of a string's payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StringEncoding {
    /// One byte per code unit; a length prefix (if any) counts bytes.
    Utf8,
    /// Two little-endian bytes per code unit; a length prefix counts units.
    Utf16,
}

/// An explicit, engine-agnostic string layout. Every axis a string
/// representation actually varies on — nothing IL2CPP-specific baked in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StringLayout {
    /// Payload encoding.
    pub encoding: StringEncoding,
    /// Byte offset, from the string object, of a 32-bit length prefix. Absent
    /// means the string is **NUL-terminated** (a native/C string). Present is the
    /// managed shape (IL2CPP/Mono/.NET store an explicit length).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "hexnum::de_opt_i64"
    )]
    pub len_at: Option<i64>,
    /// Byte offset, from the string object, of the first code unit.
    #[serde(
        default,
        skip_serializing_if = "is_zero",
        deserialize_with = "hexnum::de_i64"
    )]
    pub chars_at: i64,
    /// Whether the resolved address holds a *pointer* to the string object that
    /// must be dereferenced first (managed reference types) rather than being the
    /// object itself (an inline/native buffer).
    #[serde(default, skip_serializing_if = "is_false")]
    pub deref: bool,
}

/// A RIP-relative displacement decode, applied to a Tier-2 anchor *before* its
/// pointer chain is walked.
///
/// On x64 a static global is almost never named by a fixed module offset; it is
/// reached through an instruction like `48 8B 05 <disp32>` (`mov rax,
/// [rip+disp32]`), whose operand address is *the address of the next instruction
/// plus a signed 32-bit displacement*. An AOB scan lands on the instruction
/// bytes; this block says how to turn that hit into the operand's address:
///
/// ```text
/// operand = anchor + len + i32_at(anchor + disp)
/// ```
///
/// That operand address (typically a static slot holding a pointer) is then the
/// start of the watch's `offsets` chain. This is the missing glue that makes a
/// Tier-2 signature actually reach a static base on modern 64-bit builds — and
/// what lets a signature-anchored watch survive a patch, since the bytes are
/// matched wherever the loader placed them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rip {
    /// Byte offset, from the anchor (the AOB match address), of the signed
    /// 32-bit displacement field. For a plain `48 8B 05 <disp32>` matched from
    /// its first byte, this is `3`.
    #[serde(deserialize_with = "hexnum::de_i64")]
    pub disp: i64,
    /// Length of the whole instruction in bytes — the distance from the anchor
    /// to the *next* instruction, which RIP addressing is relative to. For
    /// `48 8B 05 <disp32>` this is `7`.
    #[serde(deserialize_with = "hexnum::de_i64")]
    pub len: i64,
}

/// The identity logic: how to recognise the target process. The filename is
/// just a label — everything a resolver needs to *claim* a process lives here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Match {
    /// Coarse bucket: the executable name (e.g. `"game.exe"`). The first, cheap
    /// filter — never sufficient on its own to claim a process.
    pub process: String,

    /// The module that anchors static (Tier-1) addresses. Often the same as the
    /// executable, but a value may live in a separately loaded module.
    pub module: String,

    /// Optional build/version discriminant (e.g. a PE version string). An
    /// opaque token: matched only when the backend can actually report a
    /// version for `module`; absent means "any build". The probe, not this, is
    /// the authoritative test — this only narrows the field cheaply first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// The AOB signature that **must resolve** in the target for this profile to
    /// claim it. The anti-collision core: a profile whose probe is not present
    /// in the process's memory does not fit, full stop.
    pub probe: String,
}

/// How a [collection](Watch::Collection)'s container is anchored. It is exactly
/// the two-tier distinction a scalar [`Watch`] draws — a static module base
/// (Tier-1) or an AOB signature with an optional RIP-relative decode (Tier-2) —
/// minus the value type: a collection reads *many* elements, so the element type
/// lives on the collection, not on its base. `offsets` walks from the resolved
/// anchor to the **container** (the list object / array) the collection iterates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "tier", rename_all = "lowercase")]
pub enum Base {
    /// Tier-1 base: `offsets` starts from `module`'s load base.
    Tier1 {
        /// Module whose load base anchors the chain.
        module: String,
        /// Pointer chain from the module base to the container.
        #[serde(deserialize_with = "hexnum::de_vec_i64")]
        offsets: Vec<i64>,
    },
    /// Tier-2 base: `offsets` starts from an AOB match (optionally RIP-decoded).
    Tier2 {
        /// AOB signature whose match address anchors the chain.
        anchor: String,
        /// Optional RIP-relative decode applied before the chain is walked; see
        /// [`Rip`]. Absent means the AOB hit is itself the chain start.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rip: Option<Rip>,
        /// Pointer chain from the anchor address to the container.
        #[serde(deserialize_with = "hexnum::de_vec_i64")]
        offsets: Vec<i64>,
    },
}

/// One named field of a [record](Watch::Record) — the same shape a
/// [record-valued collection element](Watch::Collection::fields) uses.
///
/// A field is resolved exactly like a scalar [`Watch`], but *relative to the
/// record's shared base*: walk [`offsets`](Field::offsets) from that base and
/// read one [`ty`](Field::ty) value. Reading every field of a record off the
/// **same base in the same tick** is what makes the record a coherent, atomic
/// sample — the one property a consumer zipping parallel collections by index
/// cannot reconstruct once the roster mutates between staggered samples.
///
/// It is also the natural place to factor a shared pointer prefix (a "dissect
/// structure" in Cheat-Engine terms): the base is resolved once and each field
/// is a short chain relative to it, rather than every field re-walking the whole
/// path from the anchor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Field {
    /// Pointer chain from the record's resolved base to this field's address.
    /// Empty means the base address itself holds the value.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "hexnum::de_vec_i64"
    )]
    pub offsets: Vec<i64>,
    /// How to interpret the bytes at the resolved address.
    #[serde(rename = "type")]
    pub ty: ValueType,
}

/// One node of a [derived](Watch::Derived) watch's **expression** — a total,
/// deliberately tiny arithmetic language carried as *data*.
///
/// The discipline is the one a [`Collection`](Watch::Collection) already
/// follows: iteration is expressed as `count`/`stride`/`element` rather than as
/// a script, and arithmetic is expressed as this tree rather than as an embedded
/// interpreter. There is no binding, no control flow, no recursion a profile can
/// author, and every node is total — the worst a malformed expression can do is
/// evaluate to [`Unavailable`](crate::engine::Value::Unavailable).
///
/// **No node touches memory.** Values arrive only through [`Ref`](Expr::Ref)
/// (another watch's reading this tick) or [`Item`](Expr::Item) (a field of the
/// current element under an `each`), and by then they are already plain numbers.
/// That single rule is what stops this from growing into an interpreter, and
/// what makes the tier engine-agnostic by construction: no offset, no signature
/// and no [`StringLayout`] appears anywhere below.
///
/// Discriminated by each node's unique key, so the JSON reads as the arithmetic
/// it denotes. Parsing is **strict**: a node must carry exactly one operator
/// key and nothing its operator does not take — see the [`Deserialize`] impl.
/// Serialized untagged, which writes exactly that shape back.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Expr {
    /// A literal number: `{"const": 6}`. Accepts the hex form every other number
    /// in a profile does (`{"const": "0x10"}`).
    Const {
        /// The literal's value.
        #[serde(rename = "const", deserialize_with = "hexnum::de_f64")]
        value: f64,
    },
    /// Another watch's value this tick: `{"watch": "hp"}`, optionally narrowed by
    /// an `index` into a [`List`](crate::engine::Value::List) and/or a `field` of
    /// a [`Map`](crate::engine::Value::Map) — `{"watch": "characters", "index": 2,
    /// "field": "base_hp"}` indexes first, then keys.
    ///
    /// The referenced watch must be declared **earlier in the array**; that rule
    /// (checked by [`Profile::validate`]) is the whole cycle-prevention story,
    /// and it is why evaluation order can simply be declaration order.
    Ref {
        /// Name of the watch to read.
        watch: String,
        /// Index into a list-valued watch. Out of range is unavailable, never a
        /// clamped neighbour.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "hexnum::de_opt_i64"
        )]
        index: Option<i64>,
        /// Field of a record-valued watch (applied after `index`, if both).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        field: Option<String>,
    },
    /// A field of the **current element**: `{"item": "base_patk"}`. Only legal
    /// under an [`each`](Watch::Derived::each) — outside one there is no element,
    /// and the profile is rejected rather than quietly evaluating to nothing.
    Item {
        /// Field name on the element.
        item: String,
    },
    /// Sum of one or more operands.
    Add {
        /// The operands; at least one.
        add: Vec<Expr>,
    },
    /// Product of one or more operands.
    Mul {
        /// The operands; at least one.
        mul: Vec<Expr>,
    },
    /// `a - b`.
    Sub {
        /// Exactly two operands.
        sub: Vec<Expr>,
    },
    /// `a / b`. A zero divisor is unavailable, never an infinity.
    Div {
        /// Exactly two operands.
        div: Vec<Expr>,
    },
    /// The smaller of some operands, **or** the smallest element of a list — see
    /// [`Extremum`] for why one key carries both.
    Min {
        /// An array (n-ary) or an object (fold).
        min: Extremum,
    },
    /// The larger of some operands, **or** the largest element of a list.
    Max {
        /// An array (n-ary) or an object (fold).
        max: Extremum,
    },
    /// Σ over a list watch: `{"sum": {"watch": "characters", "field": "hp"}}`.
    ///
    /// An empty or fully-filtered sum is `0` — "nothing matched" is a real
    /// answer — but an unreadable element *inside* the summed range makes the
    /// whole sum unavailable, because skipping it would quietly under-report.
    Sum {
        /// What to sum. See [`Fold`].
        sum: Fold,
    },
    /// How many elements of a list watch match: `{"count": {"watch": "enemies",
    /// "where": [{"field": "hp", "gt": 0}]}}`. Counting needs no value, so a
    /// [`field`](Fold::field) is ignored here.
    Count {
        /// What to count. See [`Fold`].
        count: Fold,
    },
}

/// Every key that names an expression node's operator, paired with the other
/// keys that operator accepts next to it.
const EXPR_OPERATORS: &[(&str, &[&str])] = &[
    ("const", &[]),
    ("watch", &["index", "field"]),
    ("item", &[]),
    ("add", &[]),
    ("mul", &[]),
    ("sub", &[]),
    ("div", &[]),
    ("min", &[]),
    ("max", &[]),
    ("sum", &[]),
    ("count", &[]),
];

/// Parse an expression node strictly.
///
/// A derived untagged parse takes the first variant that fits and ignores
/// whatever else the object holds. For an expression that is a hole, not
/// leniency: `{"watch": "nope", "const": 1}` parsed as the constant 1, skipping
/// the check that `nope` is declared earlier, and a typo like `{"watch": "hp",
/// "feild": "max"}` silently read the whole of `hp`. So the node must name
/// exactly one operator and carry only the keys that operator takes, and the
/// operator then picks the variant directly — which also lets an error inside
/// a fold or an operand say what was wrong instead of "matched no variant".
impl<'de> Deserialize<'de> for Expr {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;
        let node = serde_json::Value::deserialize(d)?;
        let serde_json::Value::Object(mut object) = node else {
            return Err(D::Error::custom("an expression node must be a JSON object"));
        };
        let operators: Vec<&(&str, &[&str])> = EXPR_OPERATORS
            .iter()
            .filter(|(key, _)| object.contains_key(*key))
            .collect();
        let (operator, extra) = match operators.as_slice() {
            [one] => **one,
            [] => {
                let names: Vec<&str> = EXPR_OPERATORS.iter().map(|(k, _)| *k).collect();
                return Err(D::Error::custom(format!(
                    "an expression node needs one of the keys {}",
                    names.join(", ")
                )));
            }
            many => {
                let names: Vec<&str> = many.iter().map(|(k, _)| *k).collect();
                return Err(D::Error::custom(format!(
                    "an expression node names more than one operator ({}); nest them instead",
                    names.join(", ")
                )));
            }
        };
        if let Some(unknown) = object
            .keys()
            .find(|k| k.as_str() != operator && !extra.contains(&k.as_str()))
        {
            return Err(D::Error::custom(format!(
                "unknown key {unknown:?} in a `{operator}` expression node"
            )));
        }

        // Each operand is parsed with its own type's rules; an error is
        // prefixed with the operator so a nested mistake can be found.
        let mut take = |key: &str| object.remove(key).unwrap_or(serde_json::Value::Null);
        fn parse<T, E: serde::de::Error>(
            operator: &str,
            f: impl FnOnce() -> std::result::Result<T, serde_json::Error>,
        ) -> std::result::Result<T, E> {
            f().map_err(|e| E::custom(format!("in `{operator}`: {e}")))
        }
        use serde_json::from_value;
        Ok(match operator {
            "const" => Expr::Const {
                value: parse(operator, || hexnum::de_f64(take("const")))?,
            },
            "watch" => Expr::Ref {
                watch: parse(operator, || from_value(take("watch")))?,
                index: parse(operator, || hexnum::de_opt_i64(take("index")))?,
                field: parse(operator, || from_value(take("field")))?,
            },
            "item" => Expr::Item {
                item: parse(operator, || from_value(take("item")))?,
            },
            "add" => Expr::Add {
                add: parse(operator, || from_value(take("add")))?,
            },
            "mul" => Expr::Mul {
                mul: parse(operator, || from_value(take("mul")))?,
            },
            "sub" => Expr::Sub {
                sub: parse(operator, || from_value(take("sub")))?,
            },
            "div" => Expr::Div {
                div: parse(operator, || from_value(take("div")))?,
            },
            "min" => Expr::Min {
                min: parse(operator, || from_value(take("min")))?,
            },
            "max" => Expr::Max {
                max: parse(operator, || from_value(take("max")))?,
            },
            "sum" => Expr::Sum {
                sum: parse(operator, || from_value(take("sum")))?,
            },
            "count" => Expr::Count {
                count: parse(operator, || from_value(take("count")))?,
            },
            _ => unreachable!("every key in EXPR_OPERATORS has an arm"),
        })
    }
}

/// The two shapes `min`/`max` accept, discriminated by JSON type.
///
/// Both are common and the two JSON types can never collide, so one key carries
/// both rather than inventing `min_of`/`min_over` names an author has to
/// remember: an **array** is n-ary over operands
/// (`{"max": [{"const": 0}, {"watch": "hp"}]}` clamps at zero), an **object** is
/// a fold over a list (`{"max": {"watch": "enemies", "field": "hp"}}`).
///
/// Parsed by JSON type rather than by trying each variant, so an error inside
/// either form reports itself instead of "matched no variant".
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Extremum {
    /// The n-ary form: the extremum of these operands. At least one.
    Nary(Vec<Expr>),
    /// The fold form: the extremum over a list watch's elements. Unlike a
    /// [`Sum`](Expr::Sum), an empty or fully-filtered fold is *unavailable* —
    /// there is no neutral element to return honestly.
    Fold(Fold),
}

impl<'de> Deserialize<'de> for Extremum {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;
        let value = serde_json::Value::deserialize(d)?;
        match value {
            serde_json::Value::Array(_) => serde_json::from_value(value).map(Extremum::Nary),
            serde_json::Value::Object(_) => serde_json::from_value(value).map(Extremum::Fold),
            _ => {
                return Err(D::Error::custom(
                    "`min`/`max` takes an array of operands or a fold object",
                ))
            }
        }
        .map_err(D::Error::custom)
    }
}

/// A fold over a list-valued watch: which list, which field of each element,
/// how many elements, and which of them count.
///
/// [`field`](Fold::field), [`take`](Fold::take) and [`clauses`](Fold::clauses)
/// are each optional; `field` is required when the elements are records and
/// omitted when they are scalars.
///
/// Unknown keys are rejected: a misspelt `where` or `field` would otherwise be
/// dropped, and the fold would quietly run over everything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fold {
    /// Name of the list-valued watch to fold, declared **earlier in the array**
    /// exactly like any other [`Ref`](Expr::Ref).
    pub watch: String,
    /// Field to read off each element. Omitted when the elements are scalars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// How many elements from the front to consider, itself an expression (a
    /// party's level, say). Clamped to `[0, len]`; a negative or unavailable
    /// `take` makes the fold unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take: Option<Box<Expr>>,
    /// Filter clauses, **all** of which must hold (an implicit AND). Named
    /// `where` on the wire, which is a Rust keyword.
    ///
    /// Filtering is not decoration: heterogeneous lists are the norm. A gear
    /// modifier list is polymorphic, and an entry of the wrong kind reads its
    /// neighbours' bytes as a number — without a clause on the type tag the sum
    /// silently produces garbage. The predicate compares a field against a
    /// literal and does not care where the discriminant came from; reaching it is
    /// the profile's job, exactly as for every other field.
    #[serde(rename = "where", default, skip_serializing_if = "Vec::is_empty")]
    pub clauses: Vec<Clause>,
}

/// One filter clause: a field of the element compared against a literal.
///
/// Deliberately flat — no nesting, no boolean algebra, no `or`. A profile that
/// genuinely needs `or` is a signal to reconsider the profile, not to grow the
/// language.
///
/// Parsed strictly (see the [`Deserialize`] impl): exactly one comparison, and
/// no key a clause does not take.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Clause {
    /// Field of the element to test.
    pub field: String,
    /// The comparison, flattened so a clause reads `{"field": "stat", "eq": 0}`
    /// rather than nesting an operator object.
    #[serde(flatten)]
    pub test: Compare,
}

/// The wire shape of a [`Clause`], with every comparison optional so the parse
/// can say how many were given. A flattened enum cannot be combined with
/// `deny_unknown_fields`, which is why the clause is not derived directly.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClauseRepr {
    field: String,
    eq: Option<Literal>,
    ne: Option<Literal>,
    lt: Option<Literal>,
    le: Option<Literal>,
    gt: Option<Literal>,
    ge: Option<Literal>,
}

/// Parse a clause strictly: a typo'd comparison key must fail rather than be
/// dropped, and `{"field": "hp", "gt": 0, "lt": 9}` must not quietly test only
/// one of the two bounds.
impl<'de> Deserialize<'de> for Clause {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        use serde::de::Error as _;
        let repr = ClauseRepr::deserialize(d)?;
        let tests: Vec<Compare> = [
            repr.eq.map(Compare::Eq),
            repr.ne.map(Compare::Ne),
            repr.lt.map(Compare::Lt),
            repr.le.map(Compare::Le),
            repr.gt.map(Compare::Gt),
            repr.ge.map(Compare::Ge),
        ]
        .into_iter()
        .flatten()
        .collect();
        match <[Compare; 1]>::try_from(tests) {
            Ok([test]) => Ok(Clause {
                field: repr.field,
                test,
            }),
            Err(tests) => Err(D::Error::custom(format!(
                "a `where` clause on {:?} needs exactly one of eq, ne, lt, le, gt, ge; got {}",
                repr.field,
                tests.len()
            ))),
        }
    }
}

/// The comparison a [`Clause`] applies, named by its JSON key.
///
/// Numbers compare numerically; strings compare only under `eq`/`ne`, because
/// an ordering on text would be a collation policy the engine has no business
/// having an opinion about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compare {
    /// Equal to the literal.
    Eq(Literal),
    /// Not equal to the literal.
    Ne(Literal),
    /// Strictly less than the literal (numbers only).
    Lt(Literal),
    /// Less than or equal to the literal (numbers only).
    Le(Literal),
    /// Strictly greater than the literal (numbers only).
    Gt(Literal),
    /// Greater than or equal to the literal (numbers only).
    Ge(Literal),
}

/// The right-hand side of a [`Clause`]: a bare JSON number or string.
///
/// No hex-string form here, unlike every offset in a profile: a clause's operand
/// is genuinely sometimes text (`"eq": "PlayerAddStatModifier"`), and quietly
/// reinterpreting a string as a number would make the two indistinguishable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Literal {
    /// A number, compared numerically against the field's reading.
    Num(f64),
    /// A string, compared for identity against a
    /// [`Str`](crate::engine::Value::Str) reading.
    Text(String),
}

/// One value the engine reads. The two scalar tiers differ only in how the
/// *anchor* address is found; both then walk `offsets` and read a typed value. A
/// [`Collection`](Watch::Collection) instead iterates a container into an array,
/// and a [`Record`](Watch::Record) reads named fields off a shared base. A
/// [`Derived`](Watch::Derived) watch reads no memory at all — it folds values the
/// others already produced.
///
/// `Eq` is intentionally *not* derived: `rate_hz` is a float, and the polling
/// loop only ever needs `PartialEq` (for the schedule) — never total equality.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "tier", rename_all = "lowercase")]
pub enum Watch {
    /// Tier-1: a static pointer path. The anchor is `module` base; `offsets`
    /// is the pointer chain from there (see [`MemoryBackend::resolve`]).
    ///
    /// [`MemoryBackend::resolve`]: crate::MemoryBackend::resolve
    Tier1 {
        /// Label for the value (e.g. `"hp"`), used by consumers downstream.
        name: String,
        /// Module whose load base anchors the chain.
        module: String,
        /// Pointer chain from the module base to the value's address.
        #[serde(deserialize_with = "hexnum::de_vec_i64")]
        offsets: Vec<i64>,
        /// How to interpret the bytes at the resolved address.
        #[serde(rename = "type")]
        ty: ValueType,
        /// How often the polling loop should sample this value, in hertz. A
        /// per-watch knob so fast-moving state (HP) can poll briskly while slow
        /// state (zone, party) sips. Absent means "every base tick"; the loop
        /// never samples faster than its own tick regardless.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_hz: Option<f64>,
    },
    /// Tier-2: the anchor is found by scanning for an AOB signature, then the
    /// pointer chain is walked from that address exactly as in Tier-1.
    Tier2 {
        /// Label for the value.
        name: String,
        /// AOB signature whose match address anchors the chain.
        anchor: String,
        /// Optional RIP-relative decode applied to the match address before the
        /// chain is walked. Absent means the AOB hit *is* the chain start (the
        /// bytes themselves are the data, or a nearby pointer). Present means the
        /// hit is a RIP-relative instruction whose operand address is the real
        /// start — the common shape for a static base on x64. See [`Rip`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rip: Option<Rip>,
        /// Pointer chain from the anchor address to the value's address.
        #[serde(deserialize_with = "hexnum::de_vec_i64")]
        offsets: Vec<i64>,
        /// How to interpret the bytes at the resolved address.
        #[serde(rename = "type")]
        ty: ValueType,
        /// Per-watch sample rate in hertz; see [`Watch::Tier1::rate_hz`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_hz: Option<f64>,
    },
    /// A **collection**: iterate a container (a C# `List<T>`, an array of entity
    /// pointers, …) into an *array* of typed values, without a scripting engine.
    /// Iteration is expressed as data — a count, a stride, and per-element chains
    /// — so the runtime stays structurally read-only and dependency-free.
    ///
    /// Resolution each tick: walk [`base`](Watch::Collection::base) to the
    /// container; read [`count`](Watch::Collection::count) (clamped to
    /// [`max`](Watch::Collection::max)); find the element region — the array a
    /// [`items`](Watch::Collection::items) chain points at (dereferenced), or the
    /// container itself when `items` is absent; then for `i in 0..count` read the
    /// value reached by [`element`](Watch::Collection::element) from
    /// `region + first + i*stride`. A broken element is
    /// [`Unavailable`](crate::engine::Value::Unavailable) without sinking the
    /// list; a base/count/items failure makes the whole watch unavailable, since
    /// the list can't be sized or located.
    Collection {
        /// Label for the value; the emitted snapshot value is an array.
        name: String,
        /// How to reach the container (list object / array). See [`Base`].
        base: Base,
        /// Chain from the container to the 32-bit element count. Clamped to
        /// `max`; a negative count reads as zero.
        #[serde(deserialize_with = "hexnum::de_vec_i64")]
        count: Vec<i64>,
        /// Optional chain from the container to the backing-array *pointer*,
        /// which is dereferenced to reach the elements (the C# `List<T>` shape:
        /// `list.items`). Absent means the elements live at the container itself
        /// (a bare pointer array).
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "hexnum::de_opt_vec_i64"
        )]
        items: Option<Vec<i64>>,
        /// Byte offset from the element region to element 0 — e.g. an IL2CPP
        /// array's header before its first slot. Defaults to 0.
        #[serde(
            default,
            skip_serializing_if = "is_zero",
            deserialize_with = "hexnum::de_i64"
        )]
        first: i64,
        /// Bytes between consecutive elements (a pointer array → 8).
        #[serde(deserialize_with = "hexnum::de_i64")]
        stride: i64,
        /// Per-element chain from an element slot to the element's address. Empty
        /// means the slot *is* the element's address. For a scalar element that
        /// address is read as [`ty`](Watch::Collection::ty); for a record element
        /// it is the base its [`fields`](Watch::Collection::fields) are relative
        /// to.
        #[serde(
            default,
            skip_serializing_if = "Vec::is_empty",
            deserialize_with = "hexnum::de_vec_i64"
        )]
        element: Vec<i64>,
        /// How to interpret each element's bytes, when the element is a **scalar**
        /// (a homogeneous array of one type). Mutually exclusive with
        /// [`fields`](Watch::Collection::fields): an element is either one typed
        /// value or a record, never both. Exactly one must be set — enforced when
        /// the profile is parsed (see [`Profile::from_json`]).
        #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
        ty: Option<ValueType>,
        /// When set, each element is a **record**: instead of one typed value,
        /// read these named fields relative to the element's resolved address (the
        /// same shape as [`Watch::Record`]). Mutually exclusive with
        /// [`ty`](Watch::Collection::ty). This is the `party = [{name, hp, mp}, …]`
        /// form; the join between fields is guaranteed coherent because they are
        /// read off the same element base in the same tick.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fields: Option<BTreeMap<String, Field>>,
        /// Hard cap on the element count — a garbage count can neither allocate
        /// nor loop unboundedly. Itself capped at [`MAX_COLLECTION_LEN`] when
        /// the profile is validated, since a cap of `u64::MAX` caps nothing.
        max: usize,
        /// Per-watch sample rate in hertz; see [`Watch::Tier1::rate_hz`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_hz: Option<f64>,
    },
    /// A **record**: resolve a single base, then read a handful of *named fields*
    /// relative to it, emitting a [`Value::Map`](crate::engine::Value::Map). The
    /// general one-level-of-structure primitive — `player = { hp, sp }` — and the
    /// same [`fields`](Watch::Record::fields) shape a
    /// [`Collection`](Watch::Collection) uses to make each element a record.
    ///
    /// The engine assigns no meaning to a field's name: a record is just "read
    /// these offsets off this base and keep them together", structurally
    /// identical to the top-level `label → Value` snapshot, nested once. Because
    /// every field is read off the same base in the same tick, the record is a
    /// coherent atomic sample; a broken field is
    /// [`Unavailable`](crate::engine::Value::Unavailable) in place without sinking
    /// the record, while a base that no longer resolves makes the whole record
    /// unavailable.
    Record {
        /// Label for the value; the emitted snapshot value is a map of field name
        /// to value.
        name: String,
        /// How to reach the record's base — the shared slot the fields are
        /// relative to. Anchored exactly like a scalar watch or a collection. See
        /// [`Base`].
        base: Base,
        /// The named fields, each resolved relative to `base`. Serialized as a
        /// JSON object; keys are emitted in sorted order for stable, testable
        /// output.
        fields: BTreeMap<String, Field>,
        /// Per-watch sample rate in hertz; see [`Watch::Tier1::rate_hz`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_hz: Option<f64>,
    },
    /// A **derived** value: arithmetic over what other watches already read.
    ///
    /// Games routinely *compute* the numbers a player sees instead of storing
    /// them, and the arithmetic is usually trivial while the plumbing to reach
    /// its inputs is not. Percent of max, a party total, an effective stat that
    /// is base plus equipment, a count of living enemies — each is one
    /// expression over values scry can already read, and without this tier every
    /// consumer re-implements it.
    ///
    /// **A derived watch never touches memory.** It reads only the values of
    /// other watches in the current tick, which is what keeps the tier from
    /// growing into an embedded interpreter and makes the failure story trivial:
    /// any unavailable input yields an unavailable output. It is also what makes
    /// it *engine-agnostic by construction* — everything engine-specific in scry
    /// lives in offsets, signatures and [`StringLayout`], and by the time a value
    /// reaches here it is just a number, a string, a list or a map.
    ///
    /// The whole expression is evaluated in `f64` and then coerced to
    /// [`ty`](Watch::Derived::ty): integer types truncate toward zero, and a
    /// non-finite result — or one outside the target's range — is
    /// [`Unavailable`](crate::engine::Value::Unavailable) rather than a saturated
    /// number, which would be a lie.
    Derived {
        /// Label for the value.
        name: String,
        /// The output type. **Never a string**: this is an arithmetic result, and
        /// [`Profile::validate`] rejects a string type rather than emitting a
        /// number formatted behind the consumer's back.
        #[serde(rename = "type")]
        ty: ValueType,
        /// When set, names an earlier [`Collection`](Watch::Collection) watch: the
        /// expression is evaluated **once per element** — with
        /// [`Item`](Expr::Item) reaching that element's fields — and the watch
        /// emits a [`List`](crate::engine::Value::List). An element whose
        /// expression fails is a nested `Unavailable` in place and the list still
        /// forms, exactly how a collection already treats its elements. Absent, the
        /// watch emits a single scalar.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        each: Option<String>,
        /// The expression to evaluate. See [`Expr`].
        value: Expr,
        /// Per-watch sample rate in hertz; see [`Watch::Tier1::rate_hz`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_hz: Option<f64>,
    },
}

/// Which **contract** a profile implements: the shape of the values it emits,
/// named by an [`id`](Contract::id) and a [`version`](Contract::version).
///
/// A contract is not a profile. Many profiles implement one contract — one per
/// build, one per storefront — and a new profile for a new build normally keeps
/// it, so nothing that renders the values has to change when a game patches.
/// Carrying the id alongside the version is what lets a consumer find the right
/// renderer without keying on the [`label`](Profile::label), which stays purely
/// descriptive: renaming a profile must never unbind the thing that draws it.
///
/// The engine never reads either half. See [`Profile::contract`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contract {
    /// Stable identifier of the contract, a lowercase slug (`"sea-of-stars"`).
    /// Restricted to `[a-z0-9]` and single inner dashes because it ends up in
    /// file names, URLs and cache keys on every consumer, and each of those has
    /// its own ideas about what else is safe.
    pub id: String,
    /// Semver-style `major.minor` of the contract. A minor adds; a major is
    /// anything else. See [`ContractVersion`].
    pub version: ContractVersion,
}

/// A contract's `major.minor`, written as a string (`"2.1"`) on the wire.
///
/// Only two components, on purpose. A contract describes a shape, and a shape
/// either gained something (minor) or changed in a way a reader has to know
/// about (major); there is no "patch" to a shape that leaves every reader
/// exactly where it was. A string rather than a JSON number because `2.10` and
/// `2.1` are different versions, and a float cannot tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContractVersion {
    /// Bumped by any change a reader of the previous version could trip over: a
    /// rename, a retype, a removal, or a value whose meaning changes.
    pub major: u32,
    /// Bumped by additions only: new watches, new fields inside records.
    pub minor: u32,
}

impl ContractVersion {
    /// Parse `"<major>.<minor>"`. Both parts are plain decimal digits: no sign,
    /// no whitespace and no third component, so a version reads the same to
    /// every tool that has to compare it.
    pub fn parse(text: &str) -> Option<Self> {
        let (major, minor) = text.split_once('.')?;
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if !digits(major) || !digits(minor) {
            return None;
        }
        Some(ContractVersion {
            major: major.parse().ok()?,
            minor: minor.parse().ok()?,
        })
    }
}

impl std::fmt::Display for ContractVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

impl Serialize for ContractVersion {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ContractVersion {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        ContractVersion::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "contract version must be \"<major>.<minor>\", got {text:?}"
            ))
        })
    }
}

/// Whether `id` is a contract slug: lowercase ASCII letters and digits in
/// dash-separated runs, with no leading, trailing or doubled dash.
fn is_slug(id: &str) -> bool {
    !id.is_empty()
        && id.split('-').all(|run| {
            !run.is_empty()
                && run
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// The contract a profile declares, after reconciling [`Profile::contract`]
/// with the deprecated [`Profile::contract_version`]. See
/// [`Profile::declared_contract`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclaredContract<'a> {
    /// The contract id, or `None` when only the deprecated integer was given —
    /// which names a version of *some* shape but not which one.
    pub id: Option<&'a str>,
    /// The contract version. From the deprecated integer `n` this is `n.0`.
    pub version: ContractVersion,
}

/// A complete per-game profile: identity plus the values to read.
///
/// Not `Eq` because a [`Watch`] carries a float `rate_hz`; `PartialEq` is all
/// the round-trip tests need.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Human-readable label. Purely informational — identity is the `match`
    /// block, never this. Optional so a minimal profile stays terse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    /// The **contract** this profile implements: which shape of values it
    /// emits — the set of watch names and their types — as opposed to
    /// [`Match::version`], which pins the game build the offsets were authored
    /// against. The two are orthogonal, and deliberately so: offsets move every
    /// patch, names outlive them. Several profiles, one per build, normally
    /// share one contract; it changes only when the emitted surface does.
    ///
    /// **The engine never reads this.** It exists for whoever renders the
    /// values, who needs to know which shape to expect and cannot infer it: the
    /// profile that wins is chosen by the *target's memory* (the probe test),
    /// not by the caller, so which contract came out is news only the engine can
    /// report. Carried through untouched, never acted on — reading it would be
    /// the engine forming an opinion about what a value *means*.
    ///
    /// Optional: a profile with no contract is still a valid profile for
    /// ad-hoc use at a terminal. It just has nothing a renderer could key on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<Contract>,

    /// **Deprecated** — write [`contract`](Profile::contract) instead.
    ///
    /// The integer that versioned the contract before contracts had an id.
    /// Still accepted so profiles written against 0.1.0-alpha.2 and alpha.3 keep
    /// loading, and read as `{"version": "<n>.0"}` with no id. Setting it next
    /// to `contract` is allowed only when the two agree on the major (the only
    /// thing the integer ever carried); anything else is rejected rather than
    /// guessed at, because a host would otherwise be told two different shapes.
    #[serde(
        rename = "contractVersion",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub contract_version: Option<u32>,

    /// The identity logic. Renamed because `match` is a Rust keyword.
    #[serde(rename = "match")]
    pub match_: Match,

    /// The values to read once this profile is selected.
    pub watches: Vec<Watch>,
}

impl Profile {
    /// Parse a profile from its JSON document. Parse failures surface as
    /// [`Error::BadProfile`] rather than a panic — a malformed community profile
    /// is an expected condition, not a bug.
    ///
    /// After a successful deserialize the profile is [validated](Profile::validate)
    /// for cross-field rules serde can't express on its own (a collection element
    /// is a scalar *or* a record, not both; a record has at least one field).
    pub fn from_json(s: &str) -> Result<Self> {
        let profile: Profile =
            serde_json::from_str(s).map_err(|e| Error::BadProfile(e.to_string()))?;
        profile.validate()?;
        Ok(profile)
    }

    /// Check cross-field invariants that the serde shape leaves open. Kept as a
    /// runtime pass (rather than a stricter serde encoding) so the JSON stays
    /// flat and readable and the error message can name the offending watch.
    ///
    /// - A [`Collection`](Watch::Collection) must set **exactly one** of `type`
    ///   (scalar element) or `fields` (record element) — never both, never
    ///   neither.
    /// - A [`Record`](Watch::Record) — and a record-valued collection — must
    ///   carry at least one field; an empty record reads nothing.
    /// - A [`Derived`](Watch::Derived) watch may only refer to watches declared
    ///   **earlier in the array**, must not declare a string `type`, may use
    ///   [`Item`](Expr::Item) only under an `each` naming an earlier collection,
    ///   and must give each operator the arity it takes.
    /// - A collection's `max` is at most [`MAX_COLLECTION_LEN`].
    /// - No two watches share a `name`, across every tier.
    /// - A `rate_hz`, when given, lies within [`MIN_RATE_HZ`]..=[`MAX_RATE_HZ`].
    /// - A declared [`contract`](Profile::contract) has a slug id, and agrees
    ///   with the deprecated `contractVersion` when both are given.
    pub fn validate(&self) -> Result<()> {
        self.check_contract()?;
        // The watches declared *before* the one being checked, grown as the loop
        // walks the array — because that ordering *is* the cycle-prevention story
        // for derived watches. A reference can only point upwards, so there is no
        // dependency graph to build and no topological sort to run, and
        // evaluation order falls out of declaration order.
        let mut earlier: Vec<&Watch> = Vec::with_capacity(self.watches.len());
        for w in &self.watches {
            // A name is the key a value is emitted under, and the key a derived
            // watch reads it back by. Two watches sharing one would overwrite
            // each other in the snapshot every tick, so the diff would never
            // settle, and a derived watch would read whichever ran last.
            let name = watch_name(w);
            check_rate(name, watch_rate(w))?;
            if declared_earlier(&earlier, name) {
                return Err(Error::BadProfile(format!(
                    "watch {name:?} is declared more than once; every watch needs its own name, \
                     whatever its tier"
                )));
            }
            match w {
                Watch::Collection { max, .. } if *max > MAX_COLLECTION_LEN => {
                    return Err(Error::BadProfile(format!(
                        "collection {name:?}: `max` {max} is above the ceiling of \
                         {MAX_COLLECTION_LEN}; a count that large is garbage memory, not a list"
                    )));
                }
                Watch::Collection {
                    name, ty, fields, ..
                } => match (ty, fields) {
                    (Some(_), Some(_)) => {
                        return Err(Error::BadProfile(format!(
                            "collection {name:?}: has both `type` and `fields`; an element is \
                             either a scalar (`type`) or a record (`fields`), not both"
                        )));
                    }
                    (None, None) => {
                        return Err(Error::BadProfile(format!(
                            "collection {name:?}: needs either a `type` (scalar element) or \
                             `fields` (record element)"
                        )));
                    }
                    (None, Some(fields)) if fields.is_empty() => {
                        return Err(Error::BadProfile(format!(
                            "collection {name:?}: `fields` is empty; a record element needs at \
                             least one field"
                        )));
                    }
                    _ => {}
                },
                Watch::Record { name, fields, .. } if fields.is_empty() => {
                    return Err(Error::BadProfile(format!(
                        "record {name:?}: needs at least one field"
                    )));
                }
                Watch::Derived {
                    name,
                    ty,
                    each,
                    value,
                    ..
                } => check_derived(name, *ty, each.as_deref(), value, &earlier)?,
                _ => {}
            }
            earlier.push(w);
        }
        Ok(())
    }

    /// The contract this profile declares, reconciling the current
    /// [`contract`](Profile::contract) object with the deprecated integer
    /// [`contract_version`](Profile::contract_version). `None` when it declares
    /// neither.
    ///
    /// Assumes a [validated](Profile::validate) profile, where the two cannot
    /// disagree; given both, the object wins because it is the richer of the two.
    pub fn declared_contract(&self) -> Option<DeclaredContract<'_>> {
        match (&self.contract, self.contract_version) {
            (Some(c), _) => Some(DeclaredContract {
                id: Some(&c.id),
                version: c.version,
            }),
            (None, Some(n)) => Some(DeclaredContract {
                id: None,
                version: ContractVersion { major: n, minor: 0 },
            }),
            (None, None) => None,
        }
    }

    /// The contract half of [`validate`](Profile::validate).
    fn check_contract(&self) -> Result<()> {
        if let Some(c) = &self.contract {
            if !is_slug(&c.id) {
                return Err(Error::BadProfile(format!(
                    "contract id {:?} is not a slug: use lowercase letters, digits and single \
                     dashes, e.g. \"sea-of-stars\"",
                    c.id
                )));
            }
            if let Some(n) = self.contract_version {
                if n != c.version.major {
                    return Err(Error::BadProfile(format!(
                        "`contractVersion` {n} disagrees with `contract.version` {}; drop the \
                         deprecated `contractVersion`",
                        c.version
                    )));
                }
            }
        }
        Ok(())
    }

    /// Serialize this profile to a pretty-printed JSON document.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|e| Error::BadProfile(e.to_string()))
    }
}

/// The largest `max` a [collection](Watch::Collection) may declare.
///
/// `max` exists so that a garbage count read out of a game mid-transition can
/// neither allocate nor loop without bound, and that only holds if `max` is
/// itself bounded: `18446744073709551615` would have been accepted, and then a
/// garbage count meant millions of reads a tick. 4096 is far above any party,
/// enemy list or inventory page seen in practice. It also keeps one `values`
/// line within reach of a host's line limit, since the first line carries every
/// element of every collection at once.
pub const MAX_COLLECTION_LEN: usize = 4096;

/// The slowest `rate_hz` a watch may ask for: one sample every 100 seconds.
///
/// Anything slower is indistinguishable from "read once" for a telemetry
/// stream, and the bound is what keeps `1 / rate_hz` a period a [`Duration`]
/// can hold: a rate like `1e-300` passed the old "is it positive" test and then
/// overflowed the conversion, taking the whole process down.
///
/// [`Duration`]: std::time::Duration
pub const MIN_RATE_HZ: f64 = 0.01;

/// The fastest `rate_hz` a watch may ask for. The loop never samples faster than
/// its own base tick anyway, so a higher rate could only ever be a typo.
pub const MAX_RATE_HZ: f64 = 1000.0;

/// A watch's `rate_hz`, whichever kind it is.
fn watch_rate(w: &Watch) -> Option<f64> {
    match w {
        Watch::Tier1 { rate_hz, .. }
        | Watch::Tier2 { rate_hz, .. }
        | Watch::Collection { rate_hz, .. }
        | Watch::Record { rate_hz, .. }
        | Watch::Derived { rate_hz, .. } => *rate_hz,
    }
}

/// A rate is either absent (every base tick) or a finite number of hertz inside
/// the documented range. Zero and negative rates are rejected rather than read
/// as "every tick", because that is what leaving the field out already says, and
/// a profile should not have two spellings of one thing, one of them a typo.
fn check_rate(name: &str, rate_hz: Option<f64>) -> Result<()> {
    match rate_hz {
        None => Ok(()),
        Some(hz) if (MIN_RATE_HZ..=MAX_RATE_HZ).contains(&hz) => Ok(()),
        Some(hz) => Err(Error::BadProfile(format!(
            "watch {name:?}: `rate_hz` {hz} is outside {MIN_RATE_HZ}..={MAX_RATE_HZ}; omit it to \
             sample every base tick"
        ))),
    }
}

/// The label a watch emits under, whichever kind it is.
fn watch_name(w: &Watch) -> &str {
    match w {
        Watch::Tier1 { name, .. }
        | Watch::Tier2 { name, .. }
        | Watch::Collection { name, .. }
        | Watch::Record { name, .. }
        | Watch::Derived { name, .. } => name,
    }
}

/// Whether a watch by this name was declared before the one being checked.
fn declared_earlier(earlier: &[&Watch], name: &str) -> bool {
    earlier.iter().any(|w| watch_name(w) == name)
}

/// Whether that earlier watch is a [`Collection`](Watch::Collection) — the only
/// kind an `each` may iterate, because only a collection produces the list it
/// walks element by element.
fn earlier_collection(earlier: &[&Watch], name: &str) -> bool {
    let found = earlier.iter().find(|w| watch_name(w) == name);
    matches!(found, Some(Watch::Collection { .. }))
}

/// Validate one [derived](Watch::Derived) watch against the watches declared
/// before it. Every message names the offending watch, because a profile is
/// hand-written data and "which one" is the first thing an author needs.
fn check_derived(
    name: &str,
    ty: ValueType,
    each: Option<&str>,
    value: &Expr,
    earlier: &[&Watch],
) -> Result<()> {
    if matches!(ty, ValueType::String(_)) {
        return Err(Error::BadProfile(format!(
            "derived {name:?}: `type` must be a number — a derived watch is an arithmetic \
             result, never text"
        )));
    }
    if let Some(each) = each {
        if !earlier_collection(earlier, each) {
            return Err(Error::BadProfile(format!(
                "derived {name:?}: `each` names {each:?}, which is not a `collection` watch \
                 declared earlier in the array"
            )));
        }
    }
    check_expr(name, value, earlier, each.is_some())
}

/// Walk an expression, checking every reference and every operator's arity.
/// `under_each` says whether an [`Item`](Expr::Item) node has an element to read.
fn check_expr(owner: &str, expr: &Expr, earlier: &[&Watch], under_each: bool) -> Result<()> {
    match expr {
        Expr::Const { .. } => Ok(()),
        Expr::Ref { watch, .. } => check_ref(owner, watch, earlier),
        Expr::Item { item } => {
            if under_each {
                Ok(())
            } else {
                Err(Error::BadProfile(format!(
                    "derived {owner:?}: item {item:?} needs an `each` — outside one there is \
                     no current element to read a field from"
                )))
            }
        }
        Expr::Add { add } => check_nary(owner, "add", add, earlier, under_each),
        Expr::Mul { mul } => check_nary(owner, "mul", mul, earlier, under_each),
        Expr::Sub { sub } => check_binary(owner, "sub", sub, earlier, under_each),
        Expr::Div { div } => check_binary(owner, "div", div, earlier, under_each),
        Expr::Min { min } => check_extremum(owner, "min", min, earlier, under_each),
        Expr::Max { max } => check_extremum(owner, "max", max, earlier, under_each),
        Expr::Sum { sum } => check_fold(owner, sum, earlier, under_each),
        Expr::Count { count } => check_fold(owner, count, earlier, under_each),
    }
}

/// Every `{"watch": …}` must name a watch declared **earlier in the array** —
/// the rule that makes a cycle unrepresentable rather than merely detectable.
fn check_ref(owner: &str, target: &str, earlier: &[&Watch]) -> Result<()> {
    if target == owner {
        return Err(Error::BadProfile(format!(
            "derived {owner:?}: refers to itself ({target:?}); a derived watch folds only \
             watches declared earlier in the array"
        )));
    }
    if declared_earlier(earlier, target) {
        return Ok(());
    }
    Err(Error::BadProfile(format!(
        "derived {owner:?}: refers to watch {target:?}, which is not declared earlier in the \
         array; evaluation order is declaration order, so a reference may only point upwards"
    )))
}

/// An n-ary operator (`add`/`mul`, and the array form of `min`/`max`) needs at
/// least one operand: there is no reading to report from no operands at all.
fn check_nary(
    owner: &str,
    op: &str,
    operands: &[Expr],
    earlier: &[&Watch],
    under_each: bool,
) -> Result<()> {
    if operands.is_empty() {
        return Err(Error::BadProfile(format!(
            "derived {owner:?}: `{op}` needs at least one operand"
        )));
    }
    for operand in operands {
        check_expr(owner, operand, earlier, under_each)?;
    }
    Ok(())
}

/// `sub`/`div` take exactly two operands — not "at least two", so a typo can
/// never silently fold a third away.
fn check_binary(
    owner: &str,
    op: &str,
    operands: &[Expr],
    earlier: &[&Watch],
    under_each: bool,
) -> Result<()> {
    if operands.len() != 2 {
        return Err(Error::BadProfile(format!(
            "derived {owner:?}: `{op}` takes exactly two operands, got {}",
            operands.len()
        )));
    }
    for operand in operands {
        check_expr(owner, operand, earlier, under_each)?;
    }
    Ok(())
}

/// `min`/`max` in either shape: an array of operands, or a fold over a list.
fn check_extremum(
    owner: &str,
    op: &str,
    extremum: &Extremum,
    earlier: &[&Watch],
    under_each: bool,
) -> Result<()> {
    match extremum {
        Extremum::Nary(operands) => check_nary(owner, op, operands, earlier, under_each),
        Extremum::Fold(fold) => check_fold(owner, fold, earlier, under_each),
    }
}

/// A fold's list watch obeys the same declared-earlier rule as any reference,
/// and its `take` is an expression like any other.
fn check_fold(owner: &str, fold: &Fold, earlier: &[&Watch], under_each: bool) -> Result<()> {
    check_ref(owner, &fold.watch, earlier)?;
    match &fold.take {
        Some(take) => check_expr(owner, take, earlier, under_each),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Profile {
        Profile {
            label: Some("Example Game (Steam)".to_string()),
            contract: None,
            contract_version: None,
            match_: Match {
                process: "game.exe".to_string(),
                module: "game.exe".to_string(),
                version: Some("1.4.2".to_string()),
                probe: "48 8B 05 ?? ?? ?? ?? 48 8B 88".to_string(),
            },
            watches: vec![
                Watch::Tier1 {
                    name: "hp".to_string(),
                    module: "game.exe".to_string(),
                    offsets: vec![0x1234, 0x10, 0x0],
                    ty: ValueType::I32,
                    rate_hz: Some(10.0),
                },
                Watch::Tier2 {
                    name: "score".to_string(),
                    anchor: "53 43 52 59 ?? ?? 11 22".to_string(),
                    // Left unset: this anchor's bytes are the chain start directly,
                    // and its omission from the serialized form is asserted below.
                    rip: None,
                    offsets: vec![0x8],
                    ty: ValueType::U32,
                    // Left unset: exercises the "every base tick" default and its
                    // omission from the serialized form.
                    rate_hz: None,
                },
            ],
        }
    }

    #[test]
    fn round_trips_through_json() {
        let profile = sample();
        let json = profile.to_json().expect("serialize");
        let back = Profile::from_json(&json).expect("deserialize");
        assert_eq!(profile, back, "profile changed across a JSON round-trip");
    }

    #[test]
    fn deserializes_from_hand_written_json() {
        // The exact shape a community profile author would write by hand.
        let json = r#"
        {
          "label": "Example Game (Steam)",
          "match": {
            "process": "game.exe",
            "module": "game.exe",
            "version": "1.4.2",
            "probe": "48 8B 05 ?? ?? ?? ?? 48 8B 88"
          },
          "watches": [
            { "tier": "tier1", "name": "hp", "module": "game.exe",
              "offsets": [4660, 16, 0], "type": "i32", "rate_hz": 10.0 },
            { "tier": "tier2", "name": "score",
              "anchor": "53 43 52 59 ?? ?? 11 22", "offsets": [8], "type": "u32" }
          ]
        }
        "#;
        assert_eq!(Profile::from_json(json).expect("parse"), sample());
    }

    #[test]
    fn rate_hz_is_optional_and_omitted_when_absent() {
        // The Tier-2 watch in `sample()` pins no rate, so the field must not
        // appear for it; the Tier-1 watch does, so it must.
        let json = sample().to_json().unwrap();
        assert!(
            json.contains("\"rate_hz\": 10.0"),
            "expected the pinned rate"
        );
        // Exactly one occurrence — the version-less watch stayed terse.
        assert_eq!(json.matches("rate_hz").count(), 1);
    }

    #[test]
    fn rip_is_optional_and_round_trips_when_present() {
        // The default path: no `rip` block, and it must not appear in the output.
        assert!(
            !sample().to_json().unwrap().contains("rip"),
            "a rip-less profile must not invent the field"
        );

        // A RIP-relative Tier-2 watch — the x64 static-base shape — round-trips
        // and deserializes from the exact JSON an author would hand-write.
        let json = r#"
        {
          "match": { "process": "g.exe", "module": "g.exe", "probe": "90 90" },
          "watches": [
            { "tier": "tier2", "name": "hp",
              "anchor": "48 8B 05 ?? ?? ?? ?? 48 8B 88",
              "rip": { "disp": 3, "len": 7 },
              "offsets": [16, 0], "type": "i32" }
          ]
        }
        "#;
        let p = Profile::from_json(json).expect("parse");
        assert_eq!(
            p.watches[0],
            Watch::Tier2 {
                name: "hp".to_string(),
                anchor: "48 8B 05 ?? ?? ?? ?? 48 8B 88".to_string(),
                rip: Some(Rip { disp: 3, len: 7 }),
                offsets: vec![16, 0],
                ty: ValueType::I32,
                rate_hz: None,
            }
        );
        // …and it survives a serialize round-trip unchanged.
        let back = Profile::from_json(&p.to_json().unwrap()).expect("re-parse");
        assert_eq!(p, back, "rip watch changed across a JSON round-trip");
    }

    #[test]
    fn version_is_optional() {
        let json = r#"
        {
          "match": { "process": "g.exe", "module": "g.exe", "probe": "90 90" },
          "watches": []
        }
        "#;
        let p = Profile::from_json(json).expect("parse");
        assert_eq!(p.match_.version, None);
        assert_eq!(p.label, None);
        // ...and a version-less profile round-trips without inventing the field.
        assert!(!p.to_json().unwrap().contains("version"));
    }

    #[test]
    fn contract_version_is_carried_but_never_required() {
        // Absent is the norm: a profile that says nothing about a contract is a
        // valid profile, and round-trips without the field appearing.
        let bare = r#"
        {
          "match": { "process": "g.exe", "module": "g.exe", "probe": "90 90" },
          "watches": []
        }
        "#;
        let p = Profile::from_json(bare).expect("parse");
        assert_eq!(p.contract_version, None);
        assert!(!p.to_json().unwrap().contains("contractVersion"));

        // Present, it survives the round trip verbatim under its JSON name. This
        // is the whole job: the engine has no opinion about the value, it only
        // has to not lose it between the file and the host reading the stream.
        let tagged = r#"
        {
          "label": "Example (Steam)",
          "contractVersion": 2,
          "match": { "process": "g.exe", "module": "g.exe", "version": "1.5.0", "probe": "90 90" },
          "watches": []
        }
        "#;
        let p = Profile::from_json(tagged).expect("parse");
        assert_eq!(p.contract_version, Some(2));
        // Orthogonal to the build discriminant: two profiles for two builds can
        // — and normally do — carry the same contract.
        assert_eq!(p.match_.version.as_deref(), Some("1.5.0"));
        let json = p.to_json().expect("serialize");
        assert!(json.contains("contractVersion"));
        assert_eq!(Profile::from_json(&json).expect("re-parse"), p);
    }

    #[test]
    fn contract_is_an_id_and_a_major_minor_version() {
        let json = r#"
        {
          "label": "Example (Steam 1.3)",
          "contract": { "id": "sea-of-stars", "version": "2.10" },
          "match": { "process": "g.exe", "module": "g.exe", "probe": "90 90" },
          "watches": []
        }
        "#;
        let p = Profile::from_json(json).expect("parse");
        let c = p.contract.as_ref().expect("contract");
        assert_eq!(c.id, "sea-of-stars");
        // `2.10` is not `2.1`: the reason the version is a string, not a float.
        assert_eq!(
            c.version,
            ContractVersion {
                major: 2,
                minor: 10
            }
        );
        assert_eq!(
            p.declared_contract(),
            Some(DeclaredContract {
                id: Some("sea-of-stars"),
                version: ContractVersion {
                    major: 2,
                    minor: 10
                },
            })
        );

        // It round-trips in the same written form, and the deprecated integer
        // does not appear out of nowhere.
        let out = p.to_json().expect("serialize");
        assert!(out.contains(r#""version": "2.10""#), "{out}");
        assert!(!out.contains("contractVersion"));
        assert_eq!(Profile::from_json(&out).expect("re-parse"), p);
    }

    #[test]
    fn contract_version_must_be_major_dot_minor() {
        for bad in [
            "2", "2.1.0", "v2.1", "2.x", "-2.1", " 2.1", "2.", ".1", "2.1 ",
        ] {
            assert_eq!(ContractVersion::parse(bad), None, "{bad:?} must not parse");
            let json = format!(
                r#"{{ "contract": {{ "id": "x", "version": "{bad}" }},
                     "match": {{ "process": "g.exe", "module": "g.exe", "probe": "90" }},
                     "watches": [] }}"#
            );
            assert!(
                Profile::from_json(&json).is_err(),
                "{bad:?} must be rejected"
            );
        }
        // A number is not accepted either, however natural `2.1` looks.
        let numeric = r#"{ "contract": { "id": "x", "version": 2.1 },
            "match": { "process": "g.exe", "module": "g.exe", "probe": "90" },
            "watches": [] }"#;
        assert!(Profile::from_json(numeric).is_err());
        assert_eq!(
            ContractVersion::parse("0.0"),
            Some(ContractVersion { major: 0, minor: 0 })
        );
    }

    #[test]
    fn contract_id_must_be_a_slug() {
        for bad in [
            "",
            "Sea-of-Stars",
            "sea of stars",
            "sea_of_stars",
            "-sea",
            "sea-",
            "sea--of",
        ] {
            let json = format!(
                r#"{{ "contract": {{ "id": "{bad}", "version": "1.0" }},
                     "match": {{ "process": "g.exe", "module": "g.exe", "probe": "90" }},
                     "watches": [] }}"#
            );
            let err = Profile::from_json(&json).expect_err(bad).to_string();
            assert!(err.contains("slug"), "{bad:?}: {err}");
        }
        for good in ["a", "sea-of-stars", "ff7-remake", "2064"] {
            let json = format!(
                r#"{{ "contract": {{ "id": "{good}", "version": "1.0" }},
                     "match": {{ "process": "g.exe", "module": "g.exe", "probe": "90" }},
                     "watches": [] }}"#
            );
            Profile::from_json(&json).expect(good);
        }
    }

    #[test]
    fn deprecated_contract_version_reads_as_major_dot_zero_with_no_id() {
        let json = r#"
        { "contractVersion": 2,
          "match": { "process": "g.exe", "module": "g.exe", "probe": "90" },
          "watches": [] }
        "#;
        let p = Profile::from_json(json).expect("parse");
        assert_eq!(p.contract, None);
        assert_eq!(
            p.declared_contract(),
            Some(DeclaredContract {
                id: None,
                version: ContractVersion { major: 2, minor: 0 },
            })
        );
    }

    #[test]
    fn both_contract_forms_must_agree_on_the_major() {
        let with = |legacy: u32, version: &str| {
            format!(
                r#"{{ "contractVersion": {legacy},
                     "contract": {{ "id": "x", "version": "{version}" }},
                     "match": {{ "process": "g.exe", "module": "g.exe", "probe": "90" }},
                     "watches": [] }}"#
            )
        };
        // The integer only ever carried the major, so it agrees with any minor
        // of it — which is what a profile migrating from one form to the other
        // naturally writes.
        let p = Profile::from_json(&with(2, "2.0")).expect("same major");
        assert_eq!(p.declared_contract().unwrap().id, Some("x"));
        Profile::from_json(&with(2, "2.3")).expect("same major, later minor");

        let err = Profile::from_json(&with(3, "2.0")).expect_err("different major");
        assert!(err.to_string().contains("disagrees"), "{err}");
    }

    #[test]
    fn offsets_accept_hex_or_decimal_interchangeably() {
        // The shape a profile pasted straight out of a disassembler takes: hex
        // strings for the numbers that were hex on screen, plain numbers where
        // decimal is natural, mixed freely — including a signed hex offset.
        let json = r#"
        {
          "match": { "process": "g", "module": "g", "probe": "90" },
          "watches": [
            { "tier": "tier1", "name": "hp", "module": "g",
              "offsets": ["0x58", 16, "0x0"], "type": "i32" },
            { "tier": "tier2", "name": "score", "anchor": "48 8B 05 ?? ?? ?? ??",
              "rip": { "disp": "0x3", "len": 7 }, "offsets": ["-0x10"], "type": "u32" }
          ]
        }
        "#;
        let p = Profile::from_json(json).expect("parse");
        match &p.watches[0] {
            Watch::Tier1 { offsets, .. } => assert_eq!(offsets, &[0x58, 16, 0x0]),
            other => panic!("expected tier1, got {other:?}"),
        }
        match &p.watches[1] {
            Watch::Tier2 { rip, offsets, .. } => {
                assert_eq!(*rip, Some(Rip { disp: 3, len: 7 }));
                assert_eq!(offsets, &[-0x10]);
            }
            other => panic!("expected tier2, got {other:?}"),
        }
    }

    #[test]
    fn an_offset_has_at_most_one_sign_and_it_comes_first() {
        use super::hexnum::parse;
        assert_eq!(parse("0x10"), Some(16));
        assert_eq!(parse("-0x10"), Some(-16));
        assert_eq!(parse("+16"), Some(16));
        assert_eq!(parse(" - 16 "), Some(-16));
        assert_eq!(parse("-0x8000000000000000"), Some(i64::MIN));
        for bad in [
            "0x-5", "0x+5", "--5", "+-5", "-+5", "0x", "-", "", "5-", "0x1g", "1_000",
        ] {
            assert_eq!(parse(bad), None, "{bad:?} must not parse");
        }
        assert_eq!(parse("0x8000000000000000"), None, "out of range");
    }

    #[test]
    fn rejects_an_unparseable_hex_offset() {
        let json = r#"
        {
          "match": { "process": "g", "module": "g", "probe": "90" },
          "watches": [
            { "tier": "tier1", "name": "x", "module": "g",
              "offsets": ["0xZZ"], "type": "i32" }
          ]
        }
        "#;
        assert!(Profile::from_json(json).is_err());
    }

    #[test]
    fn collection_watch_deserializes_and_round_trips() {
        // The C# `List<T>` shape from issue #18: a Tier-1 base into the list
        // object, count and items chains off it, a header-offset `first`, a
        // pointer stride, and a per-element chain — emitting strings.
        let json = r#"
        {
          "match": { "process": "g.exe", "module": "GameAssembly.dll", "probe": "90 90" },
          "watches": [
            { "tier": "collection", "name": "party_roster",
              "base": { "tier": "tier1", "module": "GameAssembly.dll", "offsets": ["0x38BB238", 0] },
              "count": ["0x18"], "items": ["0x10"], "first": "0x20", "stride": 8,
              "element": [0], "type": { "string": "il2cpp" }, "max": 16, "rate_hz": 4.0 }
          ]
        }
        "#;
        let p = Profile::from_json(json).expect("parse");
        assert_eq!(
            p.watches[0],
            Watch::Collection {
                name: "party_roster".to_string(),
                base: Base::Tier1 {
                    module: "GameAssembly.dll".to_string(),
                    offsets: vec![0x38BB238, 0],
                },
                count: vec![0x18],
                items: Some(vec![0x10]),
                first: 0x20,
                stride: 8,
                element: vec![0],
                ty: Some(ValueType::String(StringSpec::Preset(StringPreset::Il2cpp))),
                fields: None,
                max: 16,
                rate_hz: Some(4.0),
            }
        );
        // Survives a serialize round-trip unchanged.
        let back = Profile::from_json(&p.to_json().unwrap()).expect("re-parse");
        assert_eq!(p, back, "collection watch changed across a JSON round-trip");
    }

    #[test]
    fn string_type_accepts_preset_and_explicit_layout() {
        // The de-biased `string`: a named preset (IL2CPP is a peer, not a
        // default) and a fully explicit engine-agnostic layout (here a
        // NUL-terminated native UTF-8 string — no IL2CPP anywhere).
        let json = r#"
        {
          "match": { "process": "g", "module": "g", "probe": "90" },
          "watches": [
            { "tier": "tier1", "name": "hero", "module": "g",
              "offsets": ["0x38"], "type": { "string": "il2cpp" } },
            { "tier": "tier1", "name": "tag", "module": "g",
              "offsets": ["0x8"],
              "type": { "string": { "encoding": "utf8" } } }
          ]
        }
        "#;
        let p = Profile::from_json(json).expect("parse");
        match &p.watches[0] {
            Watch::Tier1 { ty, .. } => {
                assert_eq!(
                    *ty,
                    ValueType::String(StringSpec::Preset(StringPreset::Il2cpp))
                );
                // The preset expands to IL2CPP's concrete offsets.
                assert_eq!(
                    ty.string_layout().unwrap(),
                    StringLayout {
                        encoding: StringEncoding::Utf16,
                        len_at: Some(0x10),
                        chars_at: 0x14,
                        deref: true,
                    }
                );
            }
            other => panic!("expected tier1, got {other:?}"),
        }
        match &p.watches[1] {
            Watch::Tier1 { ty, .. } => assert_eq!(
                *ty,
                ValueType::String(StringSpec::Layout(StringLayout {
                    encoding: StringEncoding::Utf8,
                    len_at: None, // NUL-terminated
                    chars_at: 0,
                    deref: false,
                }))
            ),
            other => panic!("expected tier1, got {other:?}"),
        }
        // Round-trips (preset stays a preset, layout stays a layout).
        assert_eq!(Profile::from_json(&p.to_json().unwrap()).unwrap(), p);
        // A bare "string" is rejected — no engine is the implicit default.
        let bare = r#"{ "match": { "process": "g", "module": "g", "probe": "90" },
          "watches": [ { "tier": "tier1", "name": "x", "module": "g",
                         "offsets": [0], "type": "string" } ] }"#;
        assert!(
            Profile::from_json(bare).is_err(),
            "bare 'string' must not resolve to a default engine"
        );
    }

    #[test]
    fn collection_omits_defaulted_fields_when_absent() {
        // The bare pointer-array shape from issue #15: a Tier-2 base, no `items`
        // (elements live at the container), `first` defaulting to 0. The absent
        // fields must not be invented on serialize.
        let json = r#"
        {
          "match": { "process": "g", "module": "g", "probe": "90" },
          "watches": [
            { "tier": "collection", "name": "enemy_hp",
              "base": { "tier": "tier2", "anchor": "48 8B 05 ?? ?? ?? ??",
                        "rip": { "disp": 3, "len": 7 }, "offsets": [0] },
              "count": [16], "stride": 8, "element": [0, 88], "type": "i32", "max": 64 }
          ]
        }
        "#;
        let p = Profile::from_json(json).expect("parse");
        match &p.watches[0] {
            Watch::Collection {
                items,
                first,
                base,
                rate_hz,
                ..
            } => {
                assert_eq!(*items, None);
                assert_eq!(*first, 0);
                assert_eq!(*rate_hz, None);
                assert!(matches!(base, Base::Tier2 { rip: Some(_), .. }));
            }
            other => panic!("expected a collection, got {other:?}"),
        }
        let out = p.to_json().unwrap();
        assert!(!out.contains("items"), "absent items must not appear");
        assert!(
            !out.contains("\"first\""),
            "a zero first must stay implicit"
        );
        assert!(!out.contains("rate_hz"), "absent rate must stay implicit");
        // ...and it still round-trips.
        assert_eq!(Profile::from_json(&out).unwrap(), p);
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(Profile::from_json("{ not json").is_err());
        // Missing the required `probe` field.
        let missing_probe = r#"{ "match": { "process": "g", "module": "g" }, "watches": [] }"#;
        assert!(Profile::from_json(missing_probe).is_err());
    }

    #[test]
    fn record_watch_deserializes_and_round_trips() {
        // A top-level record: a Tier-1 base into a struct, then named fields read
        // relative to it — `player = { hp, sp }`. The exact hand-written shape.
        let json = r#"
        {
          "match": { "process": "g.exe", "module": "GameAssembly.dll", "probe": "90 90" },
          "watches": [
            { "tier": "record", "name": "player",
              "base": { "tier": "tier1", "module": "GameAssembly.dll", "offsets": ["0x2C4E120", 0] },
              "fields": {
                "hp": { "offsets": ["0x18"], "type": "i32" },
                "sp": { "offsets": ["0x1c"], "type": "i32" },
                "name": { "offsets": ["0x38"], "type": { "string": "il2cpp" } }
              },
              "rate_hz": 8.0 }
          ]
        }
        "#;
        let p = Profile::from_json(json).expect("parse");
        let mut fields = BTreeMap::new();
        fields.insert(
            "hp".to_string(),
            Field {
                offsets: vec![0x18],
                ty: ValueType::I32,
            },
        );
        fields.insert(
            "sp".to_string(),
            Field {
                offsets: vec![0x1c],
                ty: ValueType::I32,
            },
        );
        fields.insert(
            "name".to_string(),
            Field {
                offsets: vec![0x38],
                ty: ValueType::String(StringSpec::Preset(StringPreset::Il2cpp)),
            },
        );
        assert_eq!(
            p.watches[0],
            Watch::Record {
                name: "player".to_string(),
                base: Base::Tier1 {
                    module: "GameAssembly.dll".to_string(),
                    offsets: vec![0x2C4E120, 0],
                },
                fields,
                rate_hz: Some(8.0),
            }
        );
        // Survives a serialize round-trip unchanged.
        let back = Profile::from_json(&p.to_json().unwrap()).expect("re-parse");
        assert_eq!(p, back, "record watch changed across a JSON round-trip");
    }

    #[test]
    fn collection_of_records_deserializes_and_round_trips() {
        // `party = [{name, hp, mp}, …]`: the record form of a collection element.
        // `element` factors the shared deref (slot → member object); each field
        // is a short chain relative to that base.
        let json = r#"
        {
          "match": { "process": "g.exe", "module": "GameAssembly.dll", "probe": "90 90" },
          "watches": [
            { "tier": "collection", "name": "party",
              "base": { "tier": "tier1", "module": "GameAssembly.dll", "offsets": ["0x38BB238", 0] },
              "count": ["0x18"], "items": ["0x10"], "first": "0x20", "stride": 8,
              "element": [0, 0],
              "fields": {
                "name": { "offsets": ["0x38"], "type": { "string": "il2cpp" } },
                "hp": { "offsets": ["0x18"], "type": "i32" },
                "mp": { "offsets": ["0x1c"], "type": "i32" }
              },
              "max": 8 }
          ]
        }
        "#;
        let p = Profile::from_json(json).expect("parse");
        match &p.watches[0] {
            Watch::Collection { ty, fields, .. } => {
                assert_eq!(*ty, None, "a record collection carries no scalar type");
                let fields = fields.as_ref().expect("record fields");
                assert_eq!(fields.len(), 3);
                assert_eq!(fields["hp"].offsets, vec![0x18]);
                assert_eq!(fields["hp"].ty, ValueType::I32);
            }
            other => panic!("expected a collection, got {other:?}"),
        }
        // Round-trips unchanged.
        assert_eq!(Profile::from_json(&p.to_json().unwrap()).unwrap(), p);
    }

    #[test]
    fn collection_rejects_both_type_and_fields() {
        // The scalar-XOR-record rule: an element can't be both a typed value and
        // a record. Caught by validation with the watch named.
        let json = r#"
        {
          "match": { "process": "g", "module": "g", "probe": "90" },
          "watches": [
            { "tier": "collection", "name": "party",
              "base": { "tier": "tier1", "module": "g", "offsets": [0] },
              "count": [0], "stride": 8, "element": [0],
              "type": "i32",
              "fields": { "hp": { "offsets": [0], "type": "i32" } },
              "max": 8 }
          ]
        }
        "#;
        let err = Profile::from_json(json).unwrap_err().to_string();
        assert!(err.contains("party"), "error should name the watch: {err}");
        assert!(
            err.contains("both"),
            "error should explain the conflict: {err}"
        );
    }

    #[test]
    fn collection_rejects_neither_type_nor_fields() {
        let json = r#"
        {
          "match": { "process": "g", "module": "g", "probe": "90" },
          "watches": [
            { "tier": "collection", "name": "mystery",
              "base": { "tier": "tier1", "module": "g", "offsets": [0] },
              "count": [0], "stride": 8, "element": [0], "max": 8 }
          ]
        }
        "#;
        assert!(Profile::from_json(json).is_err());
    }

    /// A profile carrying one derived watch, wrapped around the minimum
    /// identity and whatever watches it needs to refer to.
    fn derived_profile(watches: &str) -> Result<Profile> {
        Profile::from_json(&format!(
            r#"{{ "match": {{ "process": "g", "module": "g", "probe": "90" }},
                  "watches": [{watches}] }}"#
        ))
    }

    /// A `collection` watch named `name` — the thing an `each` and a fold need
    /// to point at. Its offsets are irrelevant here; only validation is.
    fn a_collection(name: &str) -> String {
        format!(
            r#"{{ "tier": "collection", "name": "{name}",
                  "base": {{ "tier": "tier1", "module": "g", "offsets": [0] }},
                  "count": [0], "stride": 8, "type": "i32", "max": 8 }}"#
        )
    }

    #[test]
    fn derived_watch_deserializes_and_round_trips() {
        // The full expression surface in one watch: a literal, an indexed and
        // keyed reference, both fold shapes, a `take` that is itself an
        // expression, and a multi-clause `where` mixing a string and a number.
        let json = r#"
        {
          "match": { "process": "g.exe", "module": "GameAssembly.dll", "probe": "90 90" },
          "watches": [
            { "tier": "collection", "name": "characters",
              "base": { "tier": "tier1", "module": "GameAssembly.dll", "offsets": ["0x38BB238", 0] },
              "count": ["0x18"], "items": ["0x10"], "first": "0x20", "stride": 8,
              "element": [0, 0],
              "fields": { "hp": { "offsets": ["0x18"], "type": "i32" } }, "max": 8 },
            { "tier": "record", "name": "party_progress",
              "base": { "tier": "tier1", "module": "GameAssembly.dll", "offsets": ["0x2C4E120", 0] },
              "fields": { "level": { "offsets": ["0x18"], "type": "i32" } } },
            { "tier": "collection", "name": "levelups",
              "base": { "tier": "tier1", "module": "GameAssembly.dll", "offsets": ["0x38BB240", 0] },
              "count": ["0x18"], "items": ["0x10"], "first": "0x20", "stride": 8,
              "element": [0, 0],
              "fields": { "hp": { "offsets": ["0x10"], "type": "i32" },
                          "kind": { "offsets": ["0x38"], "type": { "string": "il2cpp" } } },
              "max": 64 },
            { "tier": "derived", "name": "max_hp", "type": "i32",
              "value": { "add": [
                { "watch": "characters", "index": 2, "field": "hp" },
                { "const": 6 },
                { "sum": { "watch": "levelups", "field": "hp",
                           "take": { "sub": [{ "watch": "party_progress", "field": "level" },
                                             { "const": 1 }] },
                           "where": [{ "field": "kind", "eq": "PlayerAddStatModifier" },
                                     { "field": "hp", "gt": 0 }] } },
                { "max": [{ "const": 0 }, { "min": { "watch": "levelups", "field": "hp" } }] }
              ] },
              "rate_hz": 2.0 },
            { "tier": "derived", "name": "attack_rating", "type": "i32", "each": "characters",
              "value": { "mul": [{ "item": "hp" }, { "const": 2 }] } }
          ]
        }
        "#;
        let p = Profile::from_json(json).expect("parse");

        // The untagged nodes discriminated the way the docs promise — in
        // particular `max` as an *array* is n-ary while `min` as an *object* is a
        // fold, which is the one place two shapes share a key.
        match &p.watches[3] {
            Watch::Derived {
                name,
                ty,
                each,
                value,
                ..
            } => {
                assert_eq!(name, "max_hp");
                assert_eq!(*ty, ValueType::I32);
                assert_eq!(*each, None);
                let operands = match value {
                    Expr::Add { add } => add,
                    other => panic!("expected an add, got {other:?}"),
                };
                assert_eq!(
                    operands[0],
                    Expr::Ref {
                        watch: "characters".to_string(),
                        index: Some(2),
                        field: Some("hp".to_string()),
                    }
                );
                assert_eq!(operands[1], Expr::Const { value: 6.0 });
                match &operands[2] {
                    Expr::Sum { sum } => {
                        assert_eq!(sum.field.as_deref(), Some("hp"));
                        assert!(sum.take.is_some(), "the take is itself an expression");
                        assert_eq!(sum.clauses.len(), 2);
                        assert_eq!(sum.clauses[0].field, "kind");
                        assert_eq!(
                            sum.clauses[0].test,
                            Compare::Eq(Literal::Text("PlayerAddStatModifier".to_string()))
                        );
                        assert_eq!(sum.clauses[1].test, Compare::Gt(Literal::Num(0.0)));
                    }
                    other => panic!("expected a sum, got {other:?}"),
                }
                match &operands[3] {
                    Expr::Max {
                        max: Extremum::Nary(nary),
                    } => match &nary[1] {
                        Expr::Min {
                            min: Extremum::Fold(_),
                        } => {}
                        other => panic!("an object min must be a fold, got {other:?}"),
                    },
                    other => panic!("an array max must be n-ary, got {other:?}"),
                }
            }
            other => panic!("expected a derived watch, got {other:?}"),
        }

        // The `each` form carries its collection through.
        match &p.watches[4] {
            Watch::Derived { each, .. } => assert_eq!(each.as_deref(), Some("characters")),
            other => panic!("expected a derived watch, got {other:?}"),
        }

        // …and the whole thing survives a serialize round-trip unchanged.
        let back = Profile::from_json(&p.to_json().unwrap()).expect("re-parse");
        assert_eq!(p, back, "derived watch changed across a JSON round-trip");
    }

    #[test]
    fn derived_rejects_a_forward_reference() {
        // `total` names a watch declared *below* it. Declaration order is
        // evaluation order, so this can never be satisfied.
        let err = derived_profile(
            r#"{ "tier": "derived", "name": "total", "type": "i32",
                 "value": { "watch": "hp" } },
               { "tier": "tier1", "name": "hp", "module": "g", "offsets": [0], "type": "i32" }"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("total"), "error should name the watch: {err}");
        assert!(err.contains("hp"), "…and the one it reached for: {err}");
    }

    #[test]
    fn derived_rejects_a_self_reference() {
        let err = derived_profile(
            r#"{ "tier": "derived", "name": "spin", "type": "i32",
                 "value": { "add": [{ "watch": "spin" }, { "const": 1 }] } }"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("spin"), "error should name the watch: {err}");
        assert!(err.contains("itself"), "…and say what it did: {err}");
    }

    #[test]
    fn derived_rejects_item_outside_an_each() {
        let err = derived_profile(
            r#"{ "tier": "derived", "name": "orphan", "type": "i32",
                 "value": { "item": "base_hp" } }"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("orphan"), "error should name the watch: {err}");
        assert!(
            err.contains("each"),
            "…and point at the missing each: {err}"
        );
    }

    #[test]
    fn derived_requires_each_to_name_an_earlier_collection() {
        // A record is not a collection: there is no list of elements to walk.
        let err = derived_profile(
            r#"{ "tier": "record", "name": "player",
                 "base": { "tier": "tier1", "module": "g", "offsets": [0] },
                 "fields": { "hp": { "offsets": [0], "type": "i32" } } },
               { "tier": "derived", "name": "doubled", "type": "i32", "each": "player",
                 "value": { "item": "hp" } }"#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("doubled"),
            "error should name the watch: {err}"
        );
        assert!(
            err.contains("player"),
            "…and what it tried to iterate: {err}"
        );

        // The same watch over a real collection is fine.
        let watches = format!(
            r#"{}, {{ "tier": "derived", "name": "doubled", "type": "i32", "each": "party",
                      "value": {{ "item": "hp" }} }}"#,
            a_collection("party")
        );
        assert!(derived_profile(&watches).is_ok());
    }

    #[test]
    fn derived_rejects_a_bad_arity() {
        // `sub` is exactly two — never three, so a stray operand can't be folded
        // away silently.
        let err = derived_profile(
            r#"{ "tier": "derived", "name": "headroom", "type": "i32",
                 "value": { "sub": [{ "const": 1 }, { "const": 2 }, { "const": 3 }] } }"#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("headroom"),
            "error should name the watch: {err}"
        );
        assert!(err.contains("sub"), "…and the operator: {err}");

        // …and an n-ary operator needs at least one operand.
        let err = derived_profile(
            r#"{ "tier": "derived", "name": "nothing", "type": "i32", "value": { "add": [] } }"#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("nothing"),
            "error should name the watch: {err}"
        );
        assert!(err.contains("add"), "…and the operator: {err}");
    }

    #[test]
    fn derived_rejects_a_string_output_type() {
        let err = derived_profile(
            r#"{ "tier": "derived", "name": "label", "type": { "string": "il2cpp" },
                 "value": { "const": 1 } }"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("label"), "error should name the watch: {err}");
        assert!(
            err.contains("type"),
            "…and say the output type is the problem: {err}"
        );
    }

    /// The two holes a lenient untagged parse left open: a second operator key
    /// silently winning over the first, and a misspelt key silently dropped.
    #[test]
    fn expressions_parse_strictly() {
        let hp =
            r#"{ "tier": "tier1", "name": "hp", "module": "g", "offsets": [0], "type": "i32" }"#;
        let with_value = |value: &str| {
            derived_profile(&format!(
                r#"{hp}, {{ "tier": "derived", "name": "d", "type": "i32", "value": {value} }}"#
            ))
        };

        // Once parsed as `const 1`, skipping the check that `nope` exists.
        let err = with_value(r#"{ "watch": "nope", "const": 1 }"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("more than one operator"), "{err}");

        // Once parsed as the whole of `hp`, the misspelt `field` dropped.
        let err = with_value(r#"{ "watch": "hp", "feild": "max" }"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown key \"feild\""), "{err}");

        let err = with_value(r#"{ "nothing": 1 }"#).unwrap_err().to_string();
        assert!(err.contains("needs one of the keys"), "{err}");

        // A fold and a clause are strict too.
        let err = with_value(r#"{ "sum": { "watch": "hp", "wehre": [] } }"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("wehre"), "{err}");
        let err = with_value(
            r#"{ "count": { "watch": "hp", "where": [{ "field": "x", "gt": 0, "lt": 9 }] } }"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("exactly one of eq"), "{err}");
        let err =
            with_value(r#"{ "count": { "watch": "hp", "where": [{ "field": "x", "eqq": 0 }] } }"#)
                .unwrap_err()
                .to_string();
        assert!(err.contains("eqq"), "{err}");

        // And the legitimate shapes still parse.
        assert!(with_value(r#"{ "watch": "hp", "index": 0, "field": "x" }"#).is_ok());
        assert!(with_value(
            r#"{ "count": { "watch": "hp", "where": [{ "field": "x", "ge": 1 }] } }"#
        )
        .is_ok());
        assert!(with_value(r#"{ "max": [{ "const": "0x10" }, { "watch": "hp" }] }"#).is_ok());

        // An error inside `min`/`max` reports itself in either form.
        let err = with_value(r#"{ "max": { "watch": "hp", "fied": "x" } }"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("fied"), "{err}");
    }

    #[test]
    fn collection_max_is_capped() {
        let with_max = |max: &str| {
            derived_profile(&format!(
                r#"{{ "tier": "collection", "name": "party",
                      "base": {{ "tier": "tier1", "module": "g", "offsets": [0] }},
                      "count": [0], "stride": 8, "type": "i32", "max": {max} }}"#
            ))
        };
        assert!(with_max("4096").is_ok());
        for bad in ["4097", "18446744073709551615"] {
            let err = with_max(bad).unwrap_err().to_string();
            assert!(
                err.contains("above the ceiling of 4096"),
                "max {bad}: {err}"
            );
        }
    }

    #[test]
    fn rate_hz_must_lie_in_the_documented_range() {
        let with_rate = |rate: &str| {
            derived_profile(&format!(
                r#"{{ "tier": "tier1", "name": "hp", "module": "g.exe", "offsets": [0],
                      "type": "i32", "rate_hz": {rate} }}"#
            ))
        };
        for ok in ["0.01", "1", "20.5", "1000"] {
            assert!(with_rate(ok).is_ok(), "rate {ok} should be accepted");
        }
        // 1e-300 once passed validation and then overflowed `1 / rate` into a
        // panic; zero and negatives are what omitting the field already says.
        for bad in ["1e-300", "0.001", "0", "-5", "1000.5", "1e300"] {
            let err = with_rate(bad).unwrap_err().to_string();
            assert!(err.contains("`rate_hz`"), "rate {bad}: {err}");
        }
    }

    #[test]
    fn rejects_two_watches_with_one_name_whatever_their_tier() {
        // Two memory watches.
        let err = derived_profile(
            r#"{ "tier": "tier1", "name": "hp", "module": "g.exe", "offsets": [0], "type": "i32" },
               { "tier": "tier1", "name": "hp", "module": "g.exe", "offsets": [4], "type": "i32" }"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("\"hp\" is declared more than once"), "{err}");

        // A derived watch reusing a memory watch's name is just as ambiguous:
        // a later reference could not say which one it meant.
        let err = derived_profile(
            r#"{ "tier": "tier1", "name": "hp", "module": "g.exe", "offsets": [0], "type": "i32" },
               { "tier": "derived", "name": "hp", "type": "i32", "value": { "const": 1 } }"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("declared more than once"), "{err}");
    }

    #[test]
    fn record_rejects_empty_fields() {
        let json = r#"
        {
          "match": { "process": "g", "module": "g", "probe": "90" },
          "watches": [
            { "tier": "record", "name": "empty",
              "base": { "tier": "tier1", "module": "g", "offsets": [0] },
              "fields": {} }
          ]
        }
        "#;
        assert!(Profile::from_json(json).is_err());
    }
}
