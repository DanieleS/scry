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

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Accept an integer offset written either as a JSON number (decimal) or as a
/// string — with an optional sign and a `0x`/`0X` prefix for hex, the notation a
/// disassembler and every memory tool actually speak. Values are stored as
/// `i64`; input is where the flexibility matters, so a profile authored from a
/// Cheat Engine session can paste `"0x58"` verbatim instead of hand-converting
/// it to `88`. (Serialization stays canonical decimal.)
mod hexnum {
    use serde::de::{self, Deserializer};
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Num(i64),
        Text(String),
    }

    /// Parse a signed decimal or `0x`-prefixed hex integer. Returns `None` on
    /// anything else, which the callers turn into a serde error.
    fn parse(text: &str) -> Option<i64> {
        let s = text.trim();
        let (sign, body) = match s.strip_prefix('-') {
            Some(rest) => (-1i64, rest.trim_start()),
            None => (1, s.strip_prefix('+').map(str::trim_start).unwrap_or(s)),
        };
        let magnitude = match body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
            Some(hex) => i64::from_str_radix(hex, 16).ok()?,
            None => body.parse::<i64>().ok()?,
        };
        Some(sign * magnitude)
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

/// One value the engine reads. The two scalar tiers differ only in how the
/// *anchor* address is found; both then walk `offsets` and read a typed value. A
/// [`Collection`](Watch::Collection) instead iterates a container into an array.
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
        /// Per-element chain from an element slot to the value's address. Empty
        /// means the slot *is* the value's address.
        #[serde(
            default,
            skip_serializing_if = "Vec::is_empty",
            deserialize_with = "hexnum::de_vec_i64"
        )]
        element: Vec<i64>,
        /// How to interpret each element's bytes.
        #[serde(rename = "type")]
        ty: ValueType,
        /// Hard cap on the element count — a garbage count can neither allocate
        /// nor loop unboundedly.
        max: usize,
        /// Per-watch sample rate in hertz; see [`Watch::Tier1::rate_hz`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_hz: Option<f64>,
    },
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
    pub fn from_json(s: &str) -> Result<Self> {
        serde_json::from_str(s).map_err(|e| Error::BadProfile(e.to_string()))
    }

    /// Serialize this profile to a pretty-printed JSON document.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|e| Error::BadProfile(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Profile {
        Profile {
            label: Some("Example Game (Steam)".to_string()),
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
                ty: ValueType::String(StringSpec::Preset(StringPreset::Il2cpp)),
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
}
