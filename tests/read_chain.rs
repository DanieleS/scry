//! End-to-end proof of the riskiest mechanic: resolve a real, module-relative
//! pointer chain in a separate process and read the value at the end of it.
//!
//! Spawns the `cavia` binary, parses the anchor facts it prints, then — from
//! the outside, exactly as the host would against a game — derives the static
//! offset, resolves `[base + player_offset] -> deref -> hp` and asserts.

use scry::aob;
use scry::{open_host, MemoryBackend};

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
fn broken_chain_errors_rather_than_lying() {
    let (_cavia, ready) = spawn_cavia();
    let be = open_host(ready.pid as u32).expect("open target");

    // A deliberately bogus first hop: dereferencing an unmapped address must
    // surface as an error, never a garbage value passed off as real.
    let bogus_start = 0xdead_0000_u64;
    let result = be.resolve(bogus_start, &[0, 0]);
    assert!(result.is_err(), "expected a read failure, got {result:?}");
}
