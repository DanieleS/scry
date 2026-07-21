//! End-to-end proof of the resolver against a real, separate process.
//!
//! The cavia plants two identity markers: a generic `PROBE` signature and a
//! build-specific `BUILD` marker. These tests attach to it exactly as the host
//! would to a game and assert the resolver's safety property: the profile that
//! *fits the memory* is chosen, and when none fits, nothing is — never a wrong
//! match.

use scry::profile::{Match, Profile};
use scry::{resolver, LinuxBackend};

mod common;
use common::spawn_cavia;

// The exact bytes the cavia plants. A profile "fits" only if its probe is
// actually one of these runs (or a wildcarded form of it).
const PROBE_SIG: &str = "50 52 4F 42 45 5F A5 5A 01 23 45 67 89 AB CD EF";
const BUILD_SIG: &str = "42 55 49 4C 44 5F 01 00 02 00 03 00";
// A run the cavia does not contain — stands in for a different game/build.
const ABSENT_SIG: &str = "0F 1E 2D 3C 4B 5A 69 78 87 96 A5 B4 C3 D2 E1 F0";

/// Build a bare identity-only profile (no watches — value reading is the
/// downstream polling loop's job, not the resolver's).
fn profile(label: &str, process: &str, version: Option<&str>, probe: &str) -> Profile {
    Profile {
        label: Some(label.to_string()),
        match_: Match {
            process: process.to_string(),
            module: process.to_string(),
            version: version.map(str::to_string),
            probe: probe.to_string(),
        },
        watches: vec![],
    }
}

fn label_of(picked: Option<&Profile>) -> Option<&str> {
    picked.and_then(|p| p.label.as_deref())
}

#[test]
fn selects_the_profile_whose_probe_resolves() {
    let (_cavia, ready) = spawn_cavia();
    let be = LinuxBackend::new(ready.pid);

    let profiles = vec![
        // Same executable, but its probe is not in the target: a lookalike.
        profile("lookalike", &ready.exe, None, ABSENT_SIG),
        // Same executable, probe present: the one that actually fits.
        profile("fits", &ready.exe, None, PROBE_SIG),
    ];

    let picked = resolver::select(&be, &ready.exe, &profiles).expect("select ok");
    assert_eq!(label_of(picked), Some("fits"));
}

#[test]
fn no_fitting_profile_yields_no_match() {
    let (_cavia, ready) = spawn_cavia();
    let be = LinuxBackend::new(ready.pid);

    // Right executable name, but the probe is absent from the memory. The
    // fail-safe: no telemetry rather than a guessed match.
    let profiles = vec![profile("wrong-build", &ready.exe, None, ABSENT_SIG)];

    let picked = resolver::select(&be, &ready.exe, &profiles).expect("select ok");
    assert!(
        picked.is_none(),
        "expected no match, got {:?}",
        label_of(picked)
    );
}

#[test]
fn same_engine_collision_is_resolved_by_the_build_marker() {
    let (_cavia, ready) = spawn_cavia();
    let be = LinuxBackend::new(ready.pid);

    // Two profiles claiming the same executable — the same-engine collision the
    // probe test exists to break. Only one probes for a marker actually present
    // in this build.
    let profiles = vec![
        profile("other-build", &ready.exe, Some("9.9.9"), ABSENT_SIG),
        profile("this-build", &ready.exe, Some("1.2.3"), BUILD_SIG),
    ];

    let picked = resolver::select(&be, &ready.exe, &profiles).expect("select ok");
    assert_eq!(label_of(picked), Some("this-build"));
}

#[test]
fn a_profile_for_another_process_is_never_chosen() {
    let (_cavia, ready) = spawn_cavia();
    let be = LinuxBackend::new(ready.pid);

    // Its probe would resolve against this memory, but the process bucket rules
    // it out first: identity starts with the executable name.
    let profiles = vec![profile(
        "some-other-game",
        "not-the-cavia.exe",
        None,
        PROBE_SIG,
    )];

    let picked = resolver::select(&be, &ready.exe, &profiles).expect("select ok");
    assert!(picked.is_none());
}

#[test]
fn probe_resolves_through_wildcards() {
    let (_cavia, ready) = spawn_cavia();
    let be = LinuxBackend::new(ready.pid);

    // The wildcarded form a real profile uses to survive across builds: the
    // volatile middle bytes are `??`, the stable ends pin the location.
    let wildcarded = "50 52 4F 42 45 5F ?? ?? ?? ?? 45 67 89 AB CD EF";
    let profiles = vec![profile("wildcarded", &ready.exe, None, wildcarded)];

    let picked = resolver::select(&be, &ready.exe, &profiles).expect("select ok");
    assert_eq!(label_of(picked), Some("wildcarded"));
}
