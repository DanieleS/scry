# scry

**A profile-driven, read-only game-memory telemetry engine.**

Give `scry` a running process and a per-game profile, and it produces snapshots
of live values — HP, mana, lap time, party state — for overlays, second screens,
and stream widgets.

---

## How it works

### The profile

A profile is a JSON document that says *how to recognise a process* and *what to
read from it*. It lives in its own file with its own update cadence — community
authors ship profiles without touching the engine.

```json
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
      "offsets": [4660, 16, 0], "type": "i32" },
    { "tier": "tier2", "name": "score",
      "anchor": "53 43 52 59 ?? ?? 11 22", "offsets": [8], "type": "u32" }
  ]
}
```

The filename is just a label. **Identity lives entirely in the `match` block** —
above all in its `probe`.

### Two tiers of watch

Both tiers walk a pointer chain and read a typed value (`i32`, `u32`, `f32`,
`u64`). They differ only in how the *anchor* address is found:

| | Anchor | Survives |
|---|---|---|
| **Tier-1** | Module load base + static offset | ASLR, restarts |
| **Tier-2** | AOB signature scan (`48 8B ?? 90`, wildcards allowed) | …and often a patch, if the signature is chosen well |

Tier-2 scanning happens **once at attach** and the result is cached — never per
poll.

### The resolver — the anti-collision core

Two games built on the same engine can share an executable name and a broad
memory shape. A name match alone would happily point telemetry at the wrong one.
The resolver refuses to guess, narrowing in three steps, cheapest first:

1. **Process bucket** — keep profiles whose `match.process` equals the running
   executable's name.
2. **Version discriminant** — if the backend can report a build version, drop
   profiles pinned to a *different* one. Profiles that don't pin a version, and
   backends that can't report one (the honest answer on Linux), are unaffected.
3. **Probe test** — the authoritative step. Scan the target for each candidate's
   `probe` signature. The profile whose probe *actually resolves in that memory*
   wins.

If no probe resolves, selection returns `None`. No telemetry, never a wrong
match. That is why emulators and unknown builds simply get nothing, at zero
cost — and why a broken community profile can't deny telemetry to a valid one
(an unparseable probe is skipped, not fatal).

---

## Design principles

- **Data over code** — a game is described by a JSON profile, not by Rust. The
  crate hard-codes no titles, so profiles ship on their own cadence.
- **Fail-safe** — a profile must fit the memory to claim a process. When nothing
  fits, you get no telemetry, never a wrong guess.
- **Host-agnostic** — the library knows nothing about streaming, clients, or
  overlays. Those live in whatever imports it.
- **Platform seam** — Linux backend (`process_vm_readv`) for dev and CI, Windows
  backend (`ReadProcessMemory`) for production. Zero external crates beyond
  serde.
- **Read-only** — the whole capability surface over a target is the
  `MemoryBackend` trait, and it has no `write`, `alloc`, or `execute`. Not a
  feature flag, not an `unsafe` escape hatch: the trait cannot express a
  mutation, so a consumer can't opt into one.

---

## Usage

```rust
use scry::{resolver, LinuxBackend, MemoryBackend, Profile};

let backend = LinuxBackend::new(pid);

// Load candidate profiles (however your host stores them).
let profiles: Vec<Profile> = load_profiles()?;

// Let the memory decide which one fits.
match resolver::select(&backend, "game.exe", &profiles)? {
    Some(profile) => {
        println!("attached with profile: {:?}", profile.label);
        // …read the profile's watches
    }
    // The fail-safe. Not an error — just nothing to report.
    None => println!("no profile fits this process"),
}
```

On Windows, swap in `WindowsBackend::open(pid)?`. Everything above the backend
seam is identical.

---

## Status

Early — version `0.0.0`, API not yet stable. What works today:

- `MemoryBackend` trait with typed reads and pointer-chain resolution
- Linux backend (`process_vm_readv`, `/proc/<pid>/maps`)
- Windows backend (`ReadProcessMemory`, module base, region enumeration, PE
  build id)
- Tier-1 module-relative pointer chains
- Tier-2 AOB signature scanning with wildcards
- Data-driven JSON profile format (serde, round-trip tested)
- Probe-based resolver with the fail-safe property
- 28 tests, zero external dependencies beyond serde, offline build

---

## Development

```sh
cargo test --lib   # unit tests: portable, run anywhere
cargo test         # + integration tests: needs Linux or Windows
```

The integration tests spawn **cavia** ("guinea pig"), a stand-in game process in
`src/bin/cavia.rs`. It reproduces the shape a real game has — a static,
module-relative slot holding a pointer to a heap struct — and plants marker byte
runs for the AOB and probe tests, then parks so the tests can read its memory
from the outside. The engine is dogfooded on the cavia itself to report its own
module base.

Note that the integration tests are gated on having a backend for the host OS;
there is no macOS backend, so `cargo test` on a Mac builds only `--lib`.

---

## License

MIT OR Apache-2.0
