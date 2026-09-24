//! The **resolver**: given a running process, pick the one profile that fits it
//! — or none, cleanly.
//!
//! This is the anti-collision core. Two games built on the same engine can
//! share an executable name and a broad memory shape; a name match alone would
//! happily point telemetry at the wrong one. The resolver refuses to guess.
//! Selection narrows in three steps, cheapest first:
//!
//! 1. **Process bucket** — keep only profiles whose `match.process` equals the
//!    running executable's name, compared ASCII case-insensitively.
//! 2. **Version discriminant** — if the backend can report a build version for
//!    the module, drop profiles that name a *different* version. Profiles that
//!    don't pin a version, and backends that can't report one, are unaffected.
//! 3. **Probe test** — the authoritative step. For each surviving candidate,
//!    scan the target for its `probe` signature. The best-ranked profile whose
//!    probe *actually resolves in that memory* wins.
//!
//! When more than one candidate fits, the choice is deterministic: a profile
//! whose `match.version` the backend **confirmed** beats one that could not be
//! confirmed (it pins no version, or the backend reports none), and otherwise
//! the earlier profile in the slice wins. A host that loads a folder should
//! therefore hand the profiles over in a stable order (the `scry` CLI sorts by
//! file name), and can call [`fitting`] to learn that the choice was a tie.
//!
//! If no candidate's probe resolves, selection returns `None`: no telemetry,
//! never a wrong match. That is the whole safety property — a profile must fit
//! the memory, not merely share a name — and it is why emulators and unknown
//! builds simply get nothing, at zero cost.

use crate::aob;
use crate::backend::MemoryBackend;
use crate::error::Result;
use crate::profile::Profile;

/// Select the profile that fits the process behind `backend`, among `profiles`.
///
/// `process` is the running executable's name (the identity the host attached
/// to). Returns `Ok(Some(profile))` for the winning profile, or `Ok(None)` when
/// nothing fits — the fail-safe. A candidate with an unparseable probe is
/// skipped rather than allowed to abort selection for the others; a broken
/// community profile must not deny telemetry to a valid one.
///
/// When several fit, the winner is the one [`fitting`] would rank first; this
/// stops scanning at that winner instead of probing the rest.
pub fn select<'a, B: MemoryBackend + ?Sized>(
    backend: &B,
    process: &str,
    profiles: &'a [Profile],
) -> Result<Option<&'a Profile>> {
    for profile in ranked_candidates(backend, process, profiles) {
        if probe_fits(backend, profile)? {
            return Ok(Some(profile));
        }
    }
    // Nothing fit the memory. Fail safe.
    Ok(None)
}

/// Every profile among `profiles` that fits the process, best first — the
/// order [`select`] picks from, so `fitting(..)[0]` is what `select` returns.
///
/// Unlike [`select`] this probes every candidate, which costs one scan of the
/// target per candidate. It is for a host that wants to know the choice was
/// not unanimous (two profiles claiming one game is worth a warning: one of
/// them is probably meant for another build), not for the hot path.
pub fn fitting<'a, B: MemoryBackend + ?Sized>(
    backend: &B,
    process: &str,
    profiles: &'a [Profile],
) -> Result<Vec<&'a Profile>> {
    let mut out = Vec::new();
    for profile in ranked_candidates(backend, process, profiles) {
        if probe_fits(backend, profile)? {
            out.push(profile);
        }
    }
    Ok(out)
}

/// Steps 1 and 2 — the process bucket and the version discriminant — with the
/// survivors ordered by the tie-break: version-confirmed first, then slice
/// order (the sort is stable).
fn ranked_candidates<'a, B: MemoryBackend + ?Sized>(
    backend: &B,
    process: &str,
    profiles: &'a [Profile],
) -> Vec<&'a Profile> {
    // 1. Process bucket: the cheap coarse filter. Case-insensitive because
    //    Windows file names are: the same game is `Game.exe` in one report and
    //    `game.exe` in another, and a profile must not miss it over that. ASCII
    //    folding is enough for executable names and needs no locale; on Linux
    //    it can only widen the bucket, and the probe still decides.
    //
    // 2. Version discriminant, only where it can be applied. We drop a
    //    candidate only when the backend reports a concrete version AND the
    //    profile pins a *different* one. Anything we can't be sure about — an
    //    unknown backend version, a version-less profile, a version read that
    //    errors — is kept, and the probe settles it.
    let mut candidates: Vec<(bool, &Profile)> = Vec::new();
    for p in profiles {
        if !p.match_.process.eq_ignore_ascii_case(process) {
            continue;
        }
        let confirmed = match (backend.module_version(&p.match_.module), &p.match_.version) {
            (Ok(Some(actual)), Some(want)) if actual != *want => continue,
            (Ok(Some(_)), Some(_)) => true,
            _ => false,
        };
        candidates.push((confirmed, p));
    }
    // A profile pinned to the exact build the backend reports was written for
    // this build; one that pins nothing was written for "any". The specific one
    // is the better guess, and it keeps a generic fallback profile from
    // shadowing a build-specific one that happens to sort after it.
    candidates.sort_by_key(|(confirmed, _)| !confirmed);
    candidates.into_iter().map(|(_, p)| p).collect()
}

/// Step 3, the probe test for one candidate. A malformed probe can never claim
/// anything, so it is "does not fit" rather than an error that would abort the
/// others.
fn probe_fits<B: MemoryBackend + ?Sized>(backend: &B, profile: &Profile) -> Result<bool> {
    match aob::parse_pattern(&profile.match_.probe) {
        Ok(pattern) => Ok(aob::find_in_process(backend, &pattern)?.is_some()),
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{MemoryBackend, Region};
    use crate::error::{Error, Result};
    use crate::profile::{Match, Profile};

    /// A deterministic in-memory backend: one readable region backed by `mem`,
    /// mapped at `base`, with a fixed (or absent) module version. Enough to
    /// exercise every branch of the resolver without spawning a process.
    struct Fake {
        base: u64,
        mem: Vec<u8>,
        version: Option<String>,
    }

    impl MemoryBackend for Fake {
        fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> Result<()> {
            let start = addr
                .checked_sub(self.base)
                .filter(|s| {
                    (*s as usize)
                        .checked_add(buf.len())
                        .is_some_and(|e| e <= self.mem.len())
                })
                .ok_or(Error::ShortRead {
                    expected: buf.len(),
                    got: 0,
                })? as usize;
            buf.copy_from_slice(&self.mem[start..start + buf.len()]);
            Ok(())
        }
        fn module_base(&self, _name: &str) -> Result<u64> {
            Ok(self.base)
        }
        fn readable_regions(&self) -> Result<Vec<Region>> {
            Ok(vec![Region {
                start: self.base,
                len: self.mem.len() as u64,
            }])
        }
        fn module_version(&self, _name: &str) -> Result<Option<String>> {
            Ok(self.version.clone())
        }
    }

    /// The unique run of bytes planted in the fake's memory. Profiles that probe
    /// for these fit; profiles that probe for anything else do not.
    const PLANTED: &str = "DE AD BE EF CA FE 12 34";

    fn fake_with_planted(version: Option<&str>) -> Fake {
        // Bury the planted signature in the middle of some filler.
        let mut mem = vec![0u8; 256];
        let sig = [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0x12, 0x34];
        mem[100..100 + sig.len()].copy_from_slice(&sig);
        Fake {
            base: 0x4000_0000,
            mem,
            version: version.map(str::to_string),
        }
    }

    fn profile(label: &str, process: &str, version: Option<&str>, probe: &str) -> Profile {
        Profile {
            label: Some(label.to_string()),
            contract: None,
            contract_version: None,
            match_: Match {
                process: process.to_string(),
                module: process.to_string(),
                version: version.map(str::to_string),
                probe: probe.to_string(),
            },
            watches: vec![],
        }
    }

    fn selected_label(picked: Option<&Profile>) -> Option<&str> {
        picked.and_then(|p| p.label.as_deref())
    }

    #[test]
    fn probe_that_resolves_wins() {
        let be = fake_with_planted(None);
        let profiles = vec![
            // Same process, but its probe is not present in memory.
            profile("wrong-build", "game.exe", None, "11 22 33 44 55 66 77 88"),
            // Same process, probe present: this is the one that fits.
            profile("right-build", "game.exe", None, PLANTED),
        ];
        let picked = select(&be, "game.exe", &profiles).expect("select ok");
        assert_eq!(selected_label(picked), Some("right-build"));
    }

    #[test]
    fn no_probe_resolves_returns_none() {
        let be = fake_with_planted(None);
        let profiles = vec![profile("nope", "game.exe", None, "11 22 33 44 55 66 77 88")];
        let picked = select(&be, "game.exe", &profiles).expect("select ok");
        assert!(picked.is_none(), "expected no match, never a wrong one");
    }

    #[test]
    fn wrong_process_name_is_filtered_before_probing() {
        let be = fake_with_planted(None);
        // Probe would resolve, but the process bucket excludes it first.
        let profiles = vec![profile("other-game", "other.exe", None, PLANTED)];
        let picked = select(&be, "game.exe", &profiles).expect("select ok");
        assert!(picked.is_none());
    }

    #[test]
    fn process_name_matches_regardless_of_ascii_case() {
        let be = fake_with_planted(None);
        let profiles = vec![profile("any-case", "Game.EXE", None, PLANTED)];
        let picked = select(&be, "game.exe", &profiles).expect("select ok");
        assert_eq!(selected_label(picked), Some("any-case"));
    }

    #[test]
    fn version_discriminant_drops_the_wrong_build() {
        let be = fake_with_planted(Some("2.0.0"));
        let profiles = vec![
            // Right process, right probe, but pinned to a version the backend
            // says this build is not.
            profile("v1", "game.exe", Some("1.0.0"), PLANTED),
            // Right process, right probe, right version.
            profile("v2", "game.exe", Some("2.0.0"), PLANTED),
        ];
        let picked = select(&be, "game.exe", &profiles).expect("select ok");
        assert_eq!(selected_label(picked), Some("v2"));
    }

    #[test]
    fn version_less_profile_survives_a_known_backend_version() {
        // A profile that doesn't pin a version is not dropped just because the
        // backend happens to know one — the probe still decides.
        let be = fake_with_planted(Some("2.0.0"));
        let profiles = vec![profile("any-build", "game.exe", None, PLANTED)];
        let picked = select(&be, "game.exe", &profiles).expect("select ok");
        assert_eq!(selected_label(picked), Some("any-build"));
    }

    #[test]
    fn unknown_backend_version_keeps_all_and_lets_probe_decide() {
        // Backend can't report a version (the Linux reality). A version-pinned
        // profile is NOT dropped on that basis — the probe is authoritative.
        let be = fake_with_planted(None);
        let profiles = vec![profile("pinned", "game.exe", Some("1.0.0"), PLANTED)];
        let picked = select(&be, "game.exe", &profiles).expect("select ok");
        assert_eq!(selected_label(picked), Some("pinned"));
    }

    #[test]
    fn several_fits_are_ranked_confirmed_version_first_then_slice_order() {
        let be = fake_with_planted(Some("2.0.0"));
        let profiles = vec![
            profile("generic-a", "game.exe", None, PLANTED),
            profile("generic-b", "game.exe", None, PLANTED),
            profile("pinned", "game.exe", Some("2.0.0"), PLANTED),
            profile("elsewhere", "game.exe", None, "11 22 33 44"),
        ];
        let all = fitting(&be, "game.exe", &profiles).expect("fitting ok");
        let labels: Vec<_> = all.iter().map(|p| p.label.as_deref().unwrap()).collect();
        assert_eq!(labels, ["pinned", "generic-a", "generic-b"]);
        // `select` picks the same winner `fitting` ranks first.
        let picked = select(&be, "game.exe", &profiles).expect("select ok");
        assert_eq!(selected_label(picked), Some("pinned"));

        // With no version to confirm, slice order alone decides.
        let be = fake_with_planted(None);
        let picked = select(&be, "game.exe", &profiles).expect("select ok");
        assert_eq!(selected_label(picked), Some("generic-a"));
    }

    #[test]
    fn malformed_probe_is_skipped_not_fatal() {
        let be = fake_with_planted(None);
        let profiles = vec![
            // Garbage probe: must not abort selection.
            profile("broken", "game.exe", None, "not hex at all"),
            // Valid, present probe: still gets to win.
            profile("good", "game.exe", None, PLANTED),
        ];
        let picked = select(&be, "game.exe", &profiles).expect("select ok");
        assert_eq!(selected_label(picked), Some("good"));
    }
}
