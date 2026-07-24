//! End-to-end proof of the `collection` watch and the `string` value type
//! against the live `cavia` process — the array/iteration path that a party or
//! enemy roster needs, and the string decode a character-identity field needs.
//!
//! The cavia plants three module-relative static slots — `enemies` (a `List` of
//! `Stats` pointers), `roster` (a `List` of `System.String` references), and
//! `name` (a lone string reference) — and reports each slot's address. From the
//! outside, exactly as a host would, we derive each slot's module offset and
//! build the watch that reads it, then assert the emitted array/value.

use scry::engine::Value;
use scry::profile::{Base, Field, Match, Profile, StringPreset, StringSpec, ValueType, Watch};
use scry::{open_host, Config, MemoryBackend, Session};
use std::collections::BTreeMap;
use std::time::Duration;

/// The IL2CPP string type (a preset) — the shape the cavia fixtures plant.
fn il2cpp_string() -> ValueType {
    ValueType::String(StringSpec::Preset(StringPreset::Il2cpp))
}

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

#[test]
fn collection_reads_an_enemy_hp_list_from_the_cavia() {
    let (_cavia, ready) = spawn_cavia();
    let be = open_host(ready.pid as u32).expect("open target");
    let base = be.module_base(&ready.exe).expect("module base");

    // The static ENEMIES slot, module-relative. The base chain adds that offset
    // (reaching the slot), derefs it (reaching the List object), and stops.
    let enemies_offset = (ready.enemies - base) as i64;

    let profile = Profile {
        label: Some("cavia (collection)".to_string()),
        match_: ident(&ready.exe),
        watches: vec![Watch::Collection {
            name: "enemy_hp".to_string(),
            base: Base::Tier1 {
                module: ready.exe.clone(),
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
        }],
    };

    let mut session = Session::attach(be, &profile, Config::default());
    let snap = session.poll(Duration::ZERO);
    assert_eq!(
        snap.get("enemy_hp"),
        Some(&Value::List(vec![
            Value::I32(11),
            Value::I32(22),
            Value::I32(33),
        ])),
        "collection watch must read the enemy HP list in order"
    );
}

#[test]
fn collection_reads_records_per_enemy_from_the_cavia() {
    // The record form of a collection, end-to-end against the live cavia: each
    // enemy element is `{hp, hp_max}` read off the same `Stats` object in one
    // tick. The cavia plants `hp_max == hp` for every enemy, so the two fields
    // pin each other — proof the record is coherent, not a stale/torn pairing.
    let (_cavia, ready) = spawn_cavia();
    let be = open_host(ready.pid as u32).expect("open target");
    let base = be.module_base(&ready.exe).expect("module base");

    let enemies_offset = (ready.enemies - base) as i64;

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

    let profile = Profile {
        label: Some("cavia (record collection)".to_string()),
        match_: ident(&ready.exe),
        watches: vec![Watch::Collection {
            name: "enemies".to_string(),
            base: Base::Tier1 {
                module: ready.exe.clone(),
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
        }],
    };

    let mut session = Session::attach(be, &profile, Config::default());
    let snap = session.poll(Duration::ZERO);
    let record = |hp: i32| {
        Value::Map(BTreeMap::from([
            ("hp".to_string(), Value::I32(hp)),
            ("hp_max".to_string(), Value::I32(hp)),
        ]))
    };
    assert_eq!(
        snap.get("enemies"),
        Some(&Value::List(vec![record(11), record(22), record(33)])),
        "a record collection must emit a coherent {{hp, hp_max}} per enemy, in order"
    );
}

#[test]
fn collection_reads_the_ordered_party_roster_as_strings() {
    let (_cavia, ready) = spawn_cavia();
    let be = open_host(ready.pid as u32).expect("open target");
    let base = be.module_base(&ready.exe).expect("module base");

    let roster_offset = (ready.roster - base) as i64;

    let profile = Profile {
        label: Some("cavia (roster)".to_string()),
        match_: ident(&ready.exe),
        watches: vec![Watch::Collection {
            name: "party".to_string(),
            base: Base::Tier1 {
                module: ready.exe.clone(),
                offsets: vec![roster_offset, 0],
            },
            count: vec![0x8],
            items: Some(vec![0x0]),
            first: 0x20,
            stride: 8,
            // The slot *is* the string reference; `read_string` derefs it.
            element: vec![],
            ty: Some(il2cpp_string()),
            fields: None,
            max: 64,
            rate_hz: None,
        }],
    };

    let mut session = Session::attach(be, &profile, Config::default());
    let snap = session.poll(Duration::ZERO);
    assert_eq!(
        snap.get("party"),
        Some(&Value::List(vec![
            Value::Str("VALERE".to_string()),
            Value::Str("ZALE".to_string()),
            Value::Str("GARL".to_string()),
        ])),
        "a single collection watch must emit the ordered, named roster"
    );
}

#[test]
fn scalar_string_watch_reads_a_character_identity() {
    let (_cavia, ready) = spawn_cavia();
    let be = open_host(ready.pid as u32).expect("open target");
    let base = be.module_base(&ready.exe).expect("module base");

    let name_offset = (ready.name - base) as i64;

    let profile = Profile {
        label: Some("cavia (string)".to_string()),
        match_: ident(&ready.exe),
        watches: vec![Watch::Tier1 {
            name: "hero".to_string(),
            module: ready.exe.clone(),
            // Resolve to the reference slot; the string type derefs from there.
            offsets: vec![name_offset],
            ty: il2cpp_string(),
            rate_hz: None,
        }],
    };

    let mut session = Session::attach(be, &profile, Config::default());
    let snap = session.poll(Duration::ZERO);
    assert_eq!(
        snap.get("hero"),
        Some(&Value::Str("ZALE".to_string())),
        "a scalar string watch must decode the referenced System.String"
    );
}
