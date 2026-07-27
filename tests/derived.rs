//! End-to-end proof of the `derived` watch against the live `cavia` process —
//! and, deliberately, the **neutrality proof** for the tier.
//!
//! The cavia is a plain Rust binary: no managed runtime, no metadata, no class
//! pointers. Its `#[repr(C)] struct Stats { hp, hp_max, frame }` sits at offsets
//! 0/4/8 behind a static slot, and its enemy list is an array of pointers to the
//! same struct. Not one assertion below involves a string layout, an array
//! header, a class pointer or anything else a managed runtime provides: every
//! input is an `i32` read out of a C-shaped struct, and every output is
//! arithmetic over those numbers.
//!
//! That is the point. A derived watch never touches memory — it folds values
//! other watches already produced — so nothing about it can be engine-specific.
//! The Sea of Stars (IL2CPP) profile is a second, harder case, not the premise.

use scry::engine::Value;
use scry::profile::{Base, Field, Match, Profile, ValueType, Watch};
use scry::{open_host, Config, MemoryBackend, Session};
use std::collections::BTreeMap;
use std::time::Duration;

mod common;
use common::spawn_cavia;

/// A minimal identity — these tests attach directly, so the `match` block is
/// never exercised against the resolver.
fn ident(exe: &str) -> Match {
    Match {
        process: exe.to_string(),
        module: exe.to_string(),
        version: None,
        probe: "90".to_string(),
    }
}

/// A Tier-1 watch reading one `i32` through the static PLAYER slot: module base
/// + slot offset, dereference, then the field's offset within `Stats`.
fn stat(name: &str, exe: &str, player_offset: i64, field: i64) -> Watch {
    Watch::Tier1 {
        name: name.to_string(),
        module: exe.to_string(),
        offsets: vec![player_offset, field],
        ty: ValueType::I32,
        rate_hz: None,
    }
}

/// A derived watch, written the way an author writes it: the expression as JSON.
fn derived(name: &str, ty: ValueType, value: &str) -> Watch {
    Watch::Derived {
        name: name.to_string(),
        ty,
        each: None,
        value: serde_json::from_str(value).expect("parse expression"),
        rate_hz: None,
    }
}

/// The enemy `List<Stats*>` as a **scalar** collection of HP values.
fn enemy_hp(exe: &str, enemies_offset: i64) -> Watch {
    Watch::Collection {
        name: "enemy_hp".to_string(),
        base: Base::Tier1 {
            module: exe.to_string(),
            offsets: vec![enemies_offset, 0],
        },
        count: vec![0x8],       // List.count
        items: Some(vec![0x0]), // List.items -> backing array
        first: 0x20,            // array header before element 0
        stride: 8,              // pointer array
        element: vec![0, 0],    // slot -> deref -> Stats -> +0 -> hp
        ty: Some(ValueType::I32),
        fields: None,
        max: 64,
        rate_hz: None,
    }
}

/// The same list as **records** — `{hp, hp_max}` per enemy — so a `where` clause
/// has a field to name.
fn enemies(exe: &str, enemies_offset: i64) -> Watch {
    let mut fields = BTreeMap::new();
    fields.insert(
        "hp".to_string(),
        Field {
            offsets: vec![0x0],
            ty: ValueType::I32,
        },
    );
    fields.insert(
        "hp_max".to_string(),
        Field {
            offsets: vec![0x4],
            ty: ValueType::I32,
        },
    );
    Watch::Collection {
        name: "enemies".to_string(),
        base: Base::Tier1 {
            module: exe.to_string(),
            offsets: vec![enemies_offset, 0],
        },
        count: vec![0x8],
        items: Some(vec![0x0]),
        first: 0x20,
        stride: 8,
        element: vec![0, 0], // slot -> deref -> Stats base
        ty: None,
        fields: Some(fields),
        max: 64,
        rate_hz: None,
    }
}

#[test]
fn derived_watches_compute_over_a_native_target() {
    let (_cavia, ready) = spawn_cavia();
    let be = open_host(ready.pid as u32).expect("open target");
    let base = be.module_base(&ready.exe).expect("module base");

    let player_offset = (ready.player - base) as i64;
    let enemies_offset = (ready.enemies - base) as i64;

    let profile = Profile {
        label: Some("cavia (derived)".to_string()),
        contract_version: None,
        match_: ident(&ready.exe),
        watches: vec![
            // The memory tier: four plain reads out of a `#[repr(C)]` struct.
            stat("hp", &ready.exe, player_offset, 0x0),
            stat("hp_max", &ready.exe, player_offset, 0x4),
            enemy_hp(&ready.exe, enemies_offset),
            enemies(&ready.exe, enemies_offset),
            // …and everything below is arithmetic over what they read.
            derived(
                "hp_percent",
                ValueType::F32,
                r#"{ "mul": [{ "const": 100 },
                             { "div": [{ "watch": "hp" }, { "watch": "hp_max" }] }] }"#,
            ),
            derived(
                "headroom",
                ValueType::I32,
                r#"{ "sub": [{ "watch": "hp_max" }, { "watch": "hp" }] }"#,
            ),
            derived(
                "enemy_hp_total",
                ValueType::I32,
                r#"{ "sum": { "watch": "enemy_hp" } }"#,
            ),
            derived(
                "enemies_alive",
                ValueType::U32,
                r#"{ "count": { "watch": "enemies",
                                "where": [{ "field": "hp", "gt": 0 }] } }"#,
            ),
            derived(
                "enemy_hp_worst",
                ValueType::I32,
                r#"{ "min": { "watch": "enemy_hp" } }"#,
            ),
        ],
    };
    // The profile is validated exactly as a loaded one would be — every
    // reference points at a watch declared above it.
    profile
        .validate()
        .expect("the derived watches must reference only earlier watches");

    let mut session = Session::attach(be, &profile, Config::default());
    let snap = session.poll(Duration::ZERO);

    // 100 * 1337 / 2000 is not exactly representable in f32 (it lands on
    // 66.849998), so compare against the f32 the arithmetic actually produces
    // rather than a formatted string.
    match snap.get("hp_percent") {
        Some(Value::F32(percent)) => assert!(
            (percent - 66.85f32).abs() < 0.001,
            "hp_percent should be ~66.85, got {percent}"
        ),
        other => panic!("expected an f32 hp_percent, got {other:?}"),
    }
    assert_eq!(
        snap.get("headroom"),
        Some(&Value::I32(663)),
        "2000 - 1337, one subtraction over two Tier-1 reads"
    );
    assert_eq!(
        snap.get("enemy_hp_total"),
        Some(&Value::I32(66)),
        "11 + 22 + 33, summed over scalar elements"
    );
    assert_eq!(
        snap.get("enemies_alive"),
        Some(&Value::U32(3)),
        "all three enemies match `hp > 0`"
    );
    assert_eq!(
        snap.get("enemy_hp_worst"),
        Some(&Value::I32(11)),
        "the fold form of min, over the same list"
    );
}

#[test]
fn a_derived_watch_re_emits_exactly_when_its_input_changes() {
    // The cavia bumps `frame` on a timer, so this is a real moving value rather
    // than a fixture poked from the test. The invariant that matters: a derived
    // watch is emitted on exactly the ticks its input changed — never stale,
    // never chattering — and it always agrees with the reading from the same
    // tick, which is what the two-phase poll exists to guarantee.
    let (_cavia, ready) = spawn_cavia();
    let be = open_host(ready.pid as u32).expect("open target");
    let base = be.module_base(&ready.exe).expect("module base");
    let player_offset = (ready.player - base) as i64;

    let profile = Profile {
        label: Some("cavia (derived, moving)".to_string()),
        contract_version: None,
        match_: ident(&ready.exe),
        watches: vec![
            stat("frame", &ready.exe, player_offset, 0x8),
            derived(
                "frame_doubled",
                ValueType::I32,
                r#"{ "mul": [{ "watch": "frame" }, { "const": 2 }] }"#,
            ),
        ],
    };

    let mut session = Session::attach(be, &profile, Config::default());
    let mut changes = 0;
    for i in 0..20u64 {
        let snap = session.poll(Duration::from_millis(i * 10));
        match (snap.get("frame"), snap.get("frame_doubled")) {
            (None, None) => {} // quiet tick: the input did not move
            (Some(Value::I32(frame)), Some(Value::I32(doubled))) => {
                assert_eq!(
                    doubled,
                    &(frame * 2),
                    "the derived value must match the tick"
                );
                changes += 1;
            }
            other => panic!("a derived watch must diff in lockstep with its input: {other:?}"),
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        changes > 0,
        "the cavia's frame counter should have moved at least once"
    );
}
