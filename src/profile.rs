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
        /// Pointer chain from the anchor address to the value's address.
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
    fn rejects_malformed_json() {
        assert!(Profile::from_json("{ not json").is_err());
        // Missing the required `probe` field.
        let missing_probe = r#"{ "match": { "process": "g", "module": "g" }, "watches": [] }"#;
        assert!(Profile::from_json(missing_probe).is_err());
    }
}
