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

### The contract — what a profile *emits*

A profile can also declare which **contract** it implements:

```json
"contract": { "id": "sea-of-stars", "version": "2.1" }
```

A contract is the **shape** of the output — the watch names and their types —
and it is orthogonal to `match.version`, which pins the *build* the offsets were
authored against:

```text
build 1.4.2 -> profile 1.4.2 -\
build 1.5.0 -> profile 1.5.0 --+-> contract sea-of-stars 1.0
build 2.0.0 -> profile 2.0.0 ---> contract sea-of-stars 2.0
```

Offsets move every patch; names outlive them. So the usual patch costs one new
profile sharing the old contract, and a consumer that renders `hp` and `party`
keeps working untouched. The version is `major.minor`: a minor only adds watches
or record fields, a major is anything else. The id is a lowercase slug, and it —
not the `label`, which stays purely descriptive — is what a renderer keys on.

The engine **never reads this field** — it parses it and hands it back on the
`attached` event, for whoever is rendering the values. An engine that acted on
it would be forming an opinion about what a value *means*, which is exactly the
line this one doesn't cross.

The integer `"contractVersion": 2` of earlier releases is still accepted but
**deprecated**: it reads as version `2.0` with no id. A profile may carry both
while migrating, as long as they agree on the major. See
[`docs/contracts-and-views.md`](docs/contracts-and-views.md) for how contracts,
profiles and the things that draw them fit together.

`scry schema <profile.json>` prints the JSON Schema of the `values` a profile
produces — one nullable property per watch, typed from its value type — which is
how a contract's schema is generated rather than written by hand.

### Two tiers of watch

Both tiers walk a pointer chain and read a typed value (`i32`, `u32`, `f32`,
`u64`, or a `string` — whose engine-agnostic layout is data: a named preset like
`il2cpp` or an explicit `{ encoding, len_at, chars_at, deref }`, length-capped).
They differ only in how the *anchor* address is found:

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

### Collections

Party and enemy **lists** need iteration, not a single read. A third watch kind,
`collection`, expresses that as **data** — a `base` chain to the container, a
`count`, a `stride`, and a per-element chain — and emits an ordered array that
diffs like any other value. No scripting engine, no new dependency; the runtime
stays structurally read-only. It reads the C# `List<T>` shape (a `count`, an
`items` backing-array pointer, a `first` header offset) or a bare pointer array,
and with `type: string` a single watch yields an ordered party roster like
`["VALERE", "ZALE", "GARL"]`. A garbage count can't run away — it's clamped to a
required `max`, itself at most 4096 — and a broken element is `unavailable` in
place without sinking the list. See [`docs/authoring-profiles.md`](docs/authoring-profiles.md).

### Records — one shallow level of structure

Sometimes a value is naturally a handful of **named fields**, not a scalar:
`player = { hp, sp }`. A `record` watch resolves a single `base`, then reads each
field as a short chain **relative to that base**, emitting a map:

```json
{ "tier": "record", "name": "player",
  "base": { "tier": "tier1", "module": "GameAssembly.dll", "offsets": ["0x2C4E120", 0] },
  "fields": {
    "hp":   { "offsets": ["0x18"], "type": "i32" },
    "sp":   { "offsets": ["0x1c"], "type": "i32" },
    "name": { "offsets": ["0x38"], "type": { "string": "il2cpp" } }
  } }
```

The same `fields` shape lets a **collection element** be a record instead of a
scalar — `party = [ {name, hp, mp}, … ]` — by giving the collection `fields`
where it would otherwise give `type`:

```json
{ "tier": "collection", "name": "party",
  "base": { … }, "count": ["0x18"], "items": ["0x10"], "first": "0x20", "stride": 8,
  "element": [0, 0],
  "fields": {
    "name": { "offsets": ["0x38"], "type": { "string": "il2cpp" } },
    "hp":   { "offsets": ["0x18"], "type": "i32" },
    "mp":   { "offsets": ["0x1c"], "type": "i32" }
  }, "max": 8 }
```

This buys two things a consumer can't get from parallel scalar collections zipped
by index. **Coherence:** every field is read off the same element base *in the
same tick*, so a roster that mutates between staggered samples can never render
one member's HP under another's name. **Factoring:** the base (and any shared
deref, like `element: [0, 0]` reaching the member object) is resolved once and
each field is a short relative chain — the "dissect structure" idea from Cheat
Engine, expressed as data. A field is `type` **xor** `fields`, never both; a
broken field is `unavailable` in place while the record still forms.

Structure stops here — **exactly one level deep**. Deeper trees (member →
inventory → items → …) stay the consumer's job to compose; recursion is where the
engine would start modelling game entities, which it deliberately doesn't.

### Derived values — arithmetic without an interpreter

Games routinely **compute** the numbers a player sees rather than storing them:
percent of max, a party total, an effective stat that is base plus equipment, a
count of living enemies. The arithmetic is trivial; reaching the inputs is not.
A `derived` watch closes that gap with a small expression carried as **data** —
the same discipline that makes `collection` express iteration as
`count`/`stride`/`element` instead of as a script:

```json
{ "tier": "derived", "name": "hp_percent", "type": "f32",
  "value": { "mul": [ { "const": 100 },
                      { "div": [ { "watch": "hp" }, { "watch": "hp_max" } ] } ] } }
```

**A derived watch never touches memory.** It reads only the values of other
watches in the current tick, which is what keeps this from growing into an
embedded interpreter — and what makes it engine-agnostic by construction, since
by the time a value gets here it is just a number, a string, a list or a map.
References must point at a watch declared **earlier** in the array, so a cycle is
unrepresentable rather than merely detected, and evaluation order is simply
declaration order.

Nodes cover literals, references (optionally `index`ed into a list and `field`ed
into a record), `add`/`mul`/`sub`/`div`/`min`/`max`, and folds over a list —
`sum`, `count`, `min`, `max` — with an optional `take` and a flat `where` filter
(`{"field": "kind", "eq": "PlayerAddStatModifier"}`) for the polymorphic lists
that are the norm rather than the exception. With `each` naming a collection, the
expression runs once per element and the watch emits a list. Everything fails
soft: any unavailable input, a zero divisor, an out-of-range coercion or a
missing field yields `unavailable` — never a saturated number, which would be a
lie. See [`docs/authoring-profiles.md`](docs/authoring-profiles.md).

### The resolver — the anti-collision core

Two games built on the same engine can share an executable name and a broad
memory shape. A name match alone would happily point telemetry at the wrong one.
The resolver refuses to guess, narrowing in three steps, cheapest first:

1. **Process bucket** — keep profiles whose `match.process` equals the running
   executable's name, ignoring ASCII case (as Windows file names do).
2. **Version discriminant** — if the backend can report a build version, drop
   profiles pinned to a *different* one. Profiles that don't pin a version, and
   backends that can't report one (the honest answer on Linux), are unaffected.
3. **Probe test** — the authoritative step. Scan the target for each candidate's
   `probe` signature. The profile whose probe *actually resolves in that memory*
   wins.

If several probes resolve, the choice is still deterministic: a profile whose
`match.version` the backend confirmed beats one that pins no version, and
otherwise the earlier profile wins. The CLI loads `--profile` files in the order
given, then a `--profiles` folder sorted by file name, and warns on stderr
naming every profile that fit.

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

For a host driving `scry` as a subprocess, `--format json` writes JSON Lines, one
event per line. The first says what was attached, including the contract the
winning profile declares (`null` when it declares none):

```json
{"event":"attached","scry":"0.1.0","pid":1234,"process":"game.exe",
 "profile":"Sea of Stars (Steam 1.3)","profile_file":"profiles/steam-1.3.json",
 "contract":{"id":"sea-of-stars","version":"2.1"},"contract_version":2,
 "watches":43,"pointer_bits":64}
{"event":"values","t_ms":5,"values":{"hp":42}}
{"event":"detached","t_ms":9000,"reason":"duration"}
```

`detached` ends the stream and says why: `once` or `duration` when the watch
ran out as asked (`--once`, `--for`), `target_exited` when the game went away —
`scry` asks the OS, since to its reads a closed game looks just like one on a
loading screen, and then exits with status 5. A host that restarts `scry`
when the game relaunches can rely on that end rather than on watches going
`null`.

`contract_version` is the same contract's major as a bare integer, kept only for
hosts that predate `contract`; it is deprecated and goes once hosts read
`contract`. A reader must ignore event types and fields it does not know.

Every `values` event is one line, and the first carries every readable watch at
once, collections and records in full. A host that caps its line length (a
1 MiB cap is common) should keep its profiles' collections well under their
4096-element ceiling: a few thousand records of a handful of strings each can
exceed it.

When `watch` fails in a way it knows about, the JSON stream ends with an `error`
event before `scry` exits, so a host can tell "no profile fits" from "could not
open the game" from a crash without parsing stderr (which says the same thing,
for a person, unchanged):

```json
{"event":"error","code":"no_profile_fits","message":"no profile fits 'game.exe' (pid 1234)","exit_code":3}
```

| `code` | Exit | Meaning |
|---|---|---|
| `usage` | 1 | The command line is wrong (a missing value, an unknown flag, a bad `--for`). |
| `no_such_process` | 2 | No running process has the `--process` name. |
| `profiles_unreadable` | 1 | A `--profile` file, or the `--profiles` folder, could not be read or parsed. A single bad file *inside* a folder is skipped with a warning instead. |
| `attach_failed` | 1 | The target could not be opened (often: it needs an elevated `scry`), or its executable name could not be read. |
| `no_profile_fits` | 3 | No profile's probe resolved in the target — the fail-safe. Nothing was read. |
| `resolver_failed` | 1 | Scanning the target for the probes failed. |
| `internal` | 101 | `scry` crashed. A bug; the panic is on stderr. |

`code` is stable; `message` is for logs and may change. `exit_code` repeats the
exit status that follows.

`scry`'s exit status, in every format:

| Exit | Meaning |
|---|---|
| 0 | Success; for `watch`, the watch ended as asked (`--once`, `--for`). |
| 1 | A usage error, an unreadable profile, a target that cannot be opened, a resolver error, a failed `selftest`, or any other error without a code of its own. |
| 2 | No running process has the `--process` name. |
| 3 | No profile fits the target. |
| 4 | `scan` found no match for the signature. |
| 5 | `watch` stopped because the target process exited. |
| 101 | A crash. |

Three more commands help author and verify:

```sh
# Find an AOB signature in a live process (for writing a profile's probe/anchor):
scry scan --process game.exe --signature "48 8B 05 ?? ?? ?? ?? 48 8B 88"

# Print the JSON Schema of the values a profile produces (any platform, no game):
scry schema game.json

# Prove the backend works on this machine — no game needed. Spawns the bundled
# cavia and checks the full read path (module base, pointer chain, AOB,
# RIP-relative decode, build id):
scry selftest
```

**Testing on Windows without building.** Every CI run publishes `scry.exe`,
`cavia.exe`, and the `il2cpp2scry.exe` converter (32- and 64-bit) as downloadable
artifacts. Grab them, drop them on a Windows box, and run `scry selftest` or
`scry watch …` against a game, and author an IL2CPP profile — no Rust toolchain,
no build pipeline.

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

Pre-release: `0.1.0` alpha builds are published from `main` (see
[`release_notes/`](release_notes/)), and the library API and profile format may
still change between alphas. The JSON event stream and the exit statuses are
kept backward compatible for hosts. What works today:

- `MemoryBackend` trait with typed reads and pointer-chain resolution
- Linux backend (`process_vm_readv`, `/proc/<pid>/maps`)
- Windows backend (`ReadProcessMemory`, module base, region enumeration, PE
  build id)
- Tier-1 module-relative pointer chains
- Tier-2 AOB signature scanning with wildcards, incl. RIP-relative (`[rip+disp32]`)
  displacement decoding to reach a static base on x64
- Data-driven JSON profile format (serde, round-trip tested)
- `collection` and `record` watches — iteration and one shallow level of
  structure, both expressed as data rather than as a script
- `derived` watches — arithmetic over what the other watches read, touching no
  memory of their own and so engine-agnostic by construction
- Probe-based resolver with the fail-safe property, deterministic when several
  profiles fit
- Contract identity (`{ id, version }`) carried from the profile to the
  `attached` event, and `scry schema` to generate a contract's JSON Schema
- `scry` host CLI — attach to a running game and stream telemetry (`watch`,
  human or JSON Lines, ending on its own when the game exits), find signatures
  (`scan`), and prove the backend end-to-end (`selftest`)
- IL2CPP profile authoring: offline `il2cpp2scry` converter (Il2CppDumper
  `dump.cs` + a name map → a profile with resolved offsets), behind a non-default
  `authoring` feature so the runtime stays engine-agnostic
- CI on Linux **and** Windows: the Windows job runs the integration suite
  against a real process (32- and 64-bit), and ships prebuilt CLI artifacts
- A unit and integration suite (the latter against a real process), plus the
  authoring converter's own; zero external dependencies beyond serde, offline
  build

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
