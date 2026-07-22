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
      "anchor": "48 8B 05 ?? ?? ?? ?? 48 8B 88", "rip": { "disp": 3, "len": 7 },
      "offsets": [16, 0], "type": "u32" }
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

On a 64-bit build a static base is rarely a fixed module offset; it is reached
through an instruction like `48 8B 05 <disp32>` (`mov rax, [rip+disp32]`), whose
operand address is *the next instruction plus a signed displacement*. A Tier-2
watch scans for that instruction and adds an optional **`rip`** block —
`{ "disp": 3, "len": 7 }` for a plain `mov` — telling the engine to decode the
displacement into the operand's address before walking `offsets`:

```text
base = anchor + len + i32_at(anchor + disp)
```

That is the glue that lets a signature-anchored watch reach a real static base
on x64 (and survive a patch, since the bytes are matched wherever the loader put
them). Omit `rip` and the AOB hit *is* the chain start, as before.

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

On Windows, swap in `WindowsBackend::open(pid)?` — or let `scry::open_host(pid)`
pick the platform backend for you. Everything above the backend seam is
identical.

---

## Command-line

The `scry` binary is a thin host over the library: point it at a **running
game**, give it a profile (or a folder of them), and it streams the live values.
It is the way to exercise the engine against a real target — above all on
Windows — without writing a host.

```sh
# Attach by name, let the resolver's probe test pick the fitting profile:
scry watch --process game.exe --profiles ./profiles/

# Attach by pid, one profile, stream for 10s:
scry watch --pid 12345 --profile game.json --for 10

# One snapshot of everything, then exit:
scry watch --process game.exe --profile game.json --once
```

Output is one line per changed value, `+<ms>  name = value`; an unchanged value
stays silent, and a value that can't be read surfaces as `unavailable` — never a
guess. If no profile's probe resolves in the target, nothing is read (the
fail-safe), and `scry` says so.

Two more commands help author and verify:

```sh
# Find an AOB signature in a live process (for writing a profile's probe/anchor):
scry scan --process game.exe --signature "48 8B 05 ?? ?? ?? ?? 48 8B 88"

# Prove the backend works on this machine — no game needed. Spawns the bundled
# cavia and checks the full read path (module base, pointer chain, AOB,
# RIP-relative decode, build id):
scry selftest
```

**Testing on Windows without building.** Every CI run publishes `scry.exe` and
`cavia.exe` (32- and 64-bit) as downloadable artifacts. Grab them, drop them on
a Windows box, and run `scry selftest` or `scry watch …` against a game — no Rust
toolchain, no build pipeline.

---

## Authoring profiles for IL2CPP games

For Unity **IL2CPP** games there's an offline converter that pins values by
**name** and derives the fragile offsets for you. A game ships its own reflection
(`global-metadata.dat` + `GameAssembly.dll`); [Il2CppDumper] parses those files —
read-only, no injection — into `class::field → offset`. Feed that dump plus a
small name map to `il2cpp2scry` and it emits a normal scry profile with the
offsets filled in:

```sh
# Built behind a non-default feature so the runtime never carries it:
cargo run --features authoring --bin il2cpp2scry -- \
    --dump dump.cs --map mygame.map.json --out mygame.json
```

The names are what you maintain; the offsets are regenerated. On a game patch you
re-run the dumper and the converter with the **same** map — no re-doing RE by
hand. All the IL2CPP knowledge lives in this offline tool, never in the read-only
telemetry runtime. See [`docs/authoring-il2cpp.md`](docs/authoring-il2cpp.md) for
the full workflow and [`examples/seaofstars/`](examples/seaofstars/) for a worked
template — and [`docs/authoring-profiles.md`](docs/authoring-profiles.md) for the
manual (Cheat Engine) route the converter builds on.

[Il2CppDumper]: https://github.com/Perfare/Il2CppDumper

---

## Status

Early — version `0.0.0`, API not yet stable. What works today:

- `MemoryBackend` trait with typed reads and pointer-chain resolution
- Linux backend (`process_vm_readv`, `/proc/<pid>/maps`)
- Windows backend (`ReadProcessMemory`, module base, region enumeration, PE
  build id)
- Tier-1 module-relative pointer chains
- Tier-2 AOB signature scanning with wildcards, incl. RIP-relative (`[rip+disp32]`)
  displacement decoding to reach a static base on x64
- Data-driven JSON profile format (serde, round-trip tested)
- Probe-based resolver with the fail-safe property
- `scry` host CLI — attach to a running game and stream telemetry (`watch`),
  find signatures (`scan`), and prove the backend end-to-end (`selftest`)
- IL2CPP profile authoring: offline `il2cpp2scry` converter (Il2CppDumper
  `dump.cs` + a name map → a profile with resolved offsets), behind a non-default
  `authoring` feature so the runtime stays engine-agnostic
- CI on Linux **and** Windows: the Windows job runs the integration suite
  against a real process (32- and 64-bit), and ships prebuilt CLI artifacts
- 42 tests, plus the authoring converter's own suite; zero external dependencies
  beyond serde, offline build

---

## Development

```sh
cargo test --lib                    # unit tests: portable, run anywhere
cargo test                          # + integration tests: needs Linux or Windows
cargo test --features authoring     # + the offline IL2CPP authoring converter
```

The integration tests spawn **cavia** ("guinea pig"), a stand-in game process in
`src/bin/cavia.rs`. It reproduces the shape a real game has — a static,
module-relative slot holding a pointer to a heap struct — plants marker byte
runs for the AOB and probe tests, and plants a real `mov rax, [rip+disp32]`
accessor pointing at that slot for the RIP-relative decode test, then parks so
the tests can read its memory from the outside. The engine is dogfooded on the cavia itself to report its own
module base.

Note that the integration tests are gated on having a backend for the host OS;
there is no macOS backend, so `cargo test` on a Mac builds only `--lib`.

---

## Direction & roadmap

- **[`docs/DIRECTION.md`](docs/DIRECTION.md)** — the north star and decision log:
  why read-only (not injection), what's been validated, the per-engine
  reflection map, and where to start next.
- **[`docs/authoring-profiles.md`](docs/authoring-profiles.md)** — how to author
  a profile from a real game (Cheat Engine → Tier-1 / Tier-2+`rip`), non-admin.
- Live work is tracked in the epic, [issue #8](https://github.com/DanieleS/scry/issues/8).

## License

MIT OR Apache-2.0
