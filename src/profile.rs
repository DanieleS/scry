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

/// One value the engine reads. The two tiers differ only in how the *anchor*
/// address is found; both then walk `offsets` and read a typed value.
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
    fn rejects_malformed_json() {
        assert!(Profile::from_json("{ not json").is_err());
        // Missing the required `probe` field.
        let missing_probe = r#"{ "match": { "process": "g", "module": "g" }, "watches": [] }"#;
        assert!(Profile::from_json(missing_probe).is_err());
    }
}
