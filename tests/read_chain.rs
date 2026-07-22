//! End-to-end proof of the riskiest mechanic: resolve a real, module-relative
//! pointer chain in a separate process and read the value at the end of it.
//!
//! Spawns the `cavia` binary, parses the anchor facts it prints, then — from
//! the outside, exactly as the host would against a game — derives the static
//! offset, resolves `[base + player_offset] -> deref -> hp` and asserts.

use scry::engine::Value;
use scry::profile::{Match, Profile, Rip, ValueType, Watch};
use scry::{aob, Config, Session};
use scry::{open_host, MemoryBackend};
use std::time::Duration;

mod common;
use common::spawn_cavia;

#[test]
fn resolves_module_relative_pointer_chain() {
    let (_cavia, ready) = spawn_cavia();
    let be = open_host(ready.pid as u32).expect("open target");

    // 1. The engine finds the same module base the target reported.
    let base = be.module_base(&ready.exe).expect("module base");
    assert_eq!(base, ready.base, "module base disagreement");

    // 2. Derive the static offset a profile would store: where PLAYER sits
    //    relative to the module load base.
    let player_offset = (ready.player - base) as i64;

    // 3. Resolve the Tier-1 path exactly as a profile describes it:
    //    start = module base, offsets = [player_offset, hp_field_offset(0)].
    //    That adds player_offset (reaching PLAYER), dereferences (reaching the
    //    heap Stats), then adds the hp field offset without a final deref.
    let hp_addr = be
        .resolve(base, &[player_offset, 0])
        .expect("resolve chain");
    let hp = be.read_i32(hp_addr).expect("read hp");

    assert_eq!(hp, ready.hp, "resolved HP mismatch");
    assert_eq!(hp, 1337, "unexpected HP value");
}

#[test]
fn aob_scan_finds_signature_in_process() {
    let (_cavia, ready) = spawn_cavia();
    let be = open_host(ready.pid as u32).expect("open target");

    // The exact bytes the cavia planted in SIG.
    let pattern = aob::parse_pattern("53 43 52 59 5A A5 11 22 33 44 55 66 77 88 99 AB").unwrap();
    let found = aob::find_in_process(&be, &pattern)
        .expect("scan ok")
        .expect("signature found");

    assert_eq!(
        found, ready.sig,
        "scan located the signature at the wrong address"
    );
}

#[test]
fn aob_scan_tolerates_wildcards() {
    let (_cavia, ready) = spawn_cavia();
    let be = open_host(ready.pid as u32).expect("open target");

    // Same signature, but with the volatile-looking middle bytes wildcarded —
    // the shape a real profile uses to survive across builds.
    let pattern = aob::parse_pattern("53 43 52 59 ?? ?? 11 22 ?? ?? 55 66 77 88 99 AB").unwrap();
    let found = aob::find_in_process(&be, &pattern)
        .expect("scan ok")
        .expect("signature found");

    assert_eq!(found, ready.sig, "wildcard scan located the wrong address");
}

#[test]
fn rip_relative_decode_recovers_the_static_slot() {
    let (_cavia, ready) = spawn_cavia();
    let be = open_host(ready.pid as u32).expect("open target");

    // The cavia planted a real `mov rax, [rip+disp32]` at `ready.rip` whose
    // operand is the PLAYER slot. Decoding it (anchor + 7 + disp32) must land
    // back on the exact address the cavia reported for PLAYER — proving the x64
    // displacement math against a genuine instruction, not a mock.
    let decoded = be.resolve_rip(ready.rip, 3, 7).expect("decode rip");
    assert_eq!(
        decoded, ready.player,
        "RIP-relative decode did not recover the PLAYER slot"
    );
}

#[test]
fn tier2_rip_relative_watch_reads_hp_end_to_end() {
    let (_cavia, ready) = spawn_cavia();
    let be = open_host(ready.pid as u32).expect("open target");

    // A full Tier-2 profile the way an author would write one for an x64 game:
    // AOB-scan the accessor, decode its RIP-relative displacement to the static
    // slot, then walk [deref PLAYER -> Stats, +0 -> hp]. No offset is known ahead
    // of time — the engine recovers the static base purely from the instruction.
    let profile = Profile {
        label: Some("cavia (rip)".to_string()),
        match_: Match {
            process: ready.exe.clone(),
            module: ready.exe.clone(),
            version: None,
            probe: "50 52 4F 42 45 5F A5 5A".to_string(), // the cavia's PROBE run
        },
        watches: vec![Watch::Tier2 {
            name: "hp".to_string(),
            anchor: "48 8B 05 ?? ?? ?? ?? C3 90 5A A5 5A A5".to_string(),
            rip: Some(Rip { disp: 3, len: 7 }),
            offsets: vec![0, 0],
            ty: ValueType::I32,
            rate_hz: None,
        }],
    };

    let mut session = Session::attach(be, &profile, Config::default());
    let snap = session.poll(Duration::ZERO);
    assert_eq!(
        snap.get("hp"),
        Some(&Value::I32(ready.hp)),
        "RIP-relative Tier-2 watch must read the live hp through the decoded base"
    );
    assert_eq!(ready.hp, 1337, "unexpected cavia hp");
}

#[test]
fn broken_chain_errors_rather_than_lying() {
    let (_cavia, ready) = spawn_cavia();
    let be = open_host(ready.pid as u32).expect("open target");

    // A deliberately bogus first hop: dereferencing an unmapped address must
    // surface as an error, never a garbage value passed off as real.
    let bogus_start = 0xdead_0000_u64;
    let result = be.resolve(bogus_start, &[0, 0]);
    assert!(result.is_err(), "expected a read failure, got {result:?}");
}
