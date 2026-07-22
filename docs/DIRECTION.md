# scry — direction & decision log

The north star for the project, and *why* it's shaped the way it is. Read this
first when picking up work. Actionable items live in GitHub issues (the epic is
[#8](https://github.com/DanieleS/scry/issues/8)); this is the reasoning behind
them.

## What scry is

A profile-driven, **read-only** game-memory telemetry engine. Given a running
game + a per-game profile, it emits diffed snapshots of live values (HP, gold,
party, enemies…) for overlays and second screens. The library is host-agnostic;
the first consumer is **Vibepollo** feeding a second screen.

## The core decision: read-only, not Cheat-Engine-style injection

We seriously weighed adopting CE's Auto-Assembler / hook approach. It is more
version-stable and would let us reuse `.CT` tables directly. We **rejected** it.
The reasoning, because it will come up again:

### Why the read-only identity is worth keeping
- It's **structural**: the `MemoryBackend` trait has no `write`/`alloc`/`exec`.
  A consumer — even a malicious community profile — literally cannot mutate the
  target. So scry **cannot crash the game**, and has the lowest possible
  anti-cheat / ToS footprint. For a telemetry/overlay that must never take the
  game down, that's the whole value proposition.
- It is **not** what gives us "no admin" (see below) — that's a separate, weaker
  property. The real, differentiating win of read-only is *can't-crash +
  minimal-footprint*. Pitch it that way.

### Why stability does **not** require injection
The fragile part of any memory read is **hardcoded offsets** (a static base RVA
+ field offsets), which move nearly every build. You can stop hardcoding them
two ways:
1. Let the game compute them → **hook** (a write/inject).
2. Ask the engine's own **reflection** for them, by name → **read-only**.

A **name outlives a code signature**: `Combatant.currentHP` survives even code
changes, whereas an AOB breaks when the surrounding code shifts. So
resolution-by-name is the *most* version-stable option **and** it's read-only.
Injection is therefore not the path to stability — reflection-by-name is.

### Why not just clone CE anyway (scope)
Executing `.CT` Auto-Assembler scripts means building an AA interpreter + an
x86-64 assembler + a memory-allocation/injection engine. That's reimplementing
CE's core — the opposite of "stay small". Importing a `.CT` is not "reading a
file"; for AA entries it's "running arbitrary assembly in the target".

**Conclusion:** keep the read-only core. Get stability from name-based
resolution. Reuse `.CT` only for the read-only-expressible subset.

## What we validated (this is empirical, not theory)

- **The idea holds against a real game.** Sea of Stars (Unity **IL2CPP**,
  `SeaOfStars.exe` + `GameAssembly.dll`) is the hard case and it exposed the one
  real gap — which we then closed (see below). Read-only was never the ceiling;
  hardcoded offsets were.
- **RIP-relative was the missing glue.** The README/design promised
  `[rip+disp32]` resolution but the code didn't have it. On x64 a static base is
  reached through `mov reg,[rip+disp32]`; without decoding the displacement,
  Tier-2 could never reach a static base. **Now implemented** (`rip { disp,
  len }`), proven end-to-end against a real accessor planted in the cavia.
- **Hex offsets.** Profiles now accept `"0x58"` as well as `88` — the notation
  every memory tool speaks; forcing decimal-by-hand was an error trap.
- **"No admin" is real but not a differentiator.** A same-user, medium-integrity
  single-player game is readable *and writable* without admin (field-checked:
  non-admin Cheat Engine attached to Sea of Stars and worked). Admin depends on
  the **target's integrity/owner/protection**, not on read-vs-write. Drop "no
  admin needed" as a selling point — it's just a fact, and a writer gets it too.

## Engine coverage map (read-only reflection)

Resilience tracks whether the engine ships runtime reflection. This is the map
for the by-name resolver ([#14](https://github.com/DanieleS/scry/issues/14)).

| Engine | Read-only mechanism | Resilience | Example |
|---|---|---|---|
| **Unity IL2CPP** | `global-metadata.dat` + `GameAssembly.dll` (find registration via RIP-relative) | High | **Sea of Stars** |
| **Unity Mono** | `MonoClass` / `MonoClassField` structs from the domain | High | many Unity titles |
| **.NET / CLR** | **DAC / ClrMD** (`mscordaccore`), out-of-process, vendor-blessed | Very high | MonoGame/FNA — *The Messenger* (ex-Sabotage) |
| **Unreal** | `GNames` + `GUObjectArray` + `FProperty` chain | High | **Octopath Traveler 2** |
| **Godot** | `ClassDB` | Medium | — |
| **native C++ bespoke** | none → AOB + `rip` + offsets (fragile) | Low | Source, id Tech, custom |

Practical takeaway: the first by-name backend to build is **IL2CPP** (widest
reach, metadata ships as files). CLR is the most robust where applicable.

## Keeping the runtime small

The recurring worry: "a backend per engine and it's not small anymore." The
answer is a **seam**: engine-specific knowledge lives in **offline authoring
tools** that emit a profile with *resolved* offsets; the runtime keeps reading
plain offsets/signatures, engine-agnostic, exactly as today.

- Near-term ([#13](https://github.com/DanieleS/scry/issues/13)): an IL2CPP
  *converter* (Il2CppDumper output + chosen names → profile). Runtime untouched;
  per-build resilience = re-run the dumper.
- Later ([#14](https://github.com/DanieleS/scry/issues/14)): move resolution
  *into* the runtime behind a pluggable `Resolver` trait, for automatic
  per-build re-resolution. Bigger; do it only if the spike justifies it.

## Roadmap (see the epic for live status)

1. **[#12] Spike** — measure read-only reusability of real `.CT` tables. Gate.
2. **[#13] IL2CPP → profile converter** — first real profile: Sea of Stars.
3. **[#15] Data-driven collection watch** — party/enemy lists, no scripting VM.
4. **[#6] `.CT` importer** — read-only subset, scoped by #12.
5. **[#7] Versioned JSON Schema** — the community contract.
6. **[#14] Runtime by-name reflection resolver** — the resilient endgame.
7. **[#5] Vibepollo integration** — adapter + control-channel `0x3003`.

## Where to start next session

Two entry points, both against **Sea of Stars** (our confirmed IL2CPP target):
- **#12** — categorise its real `.CT` (pointer vs AA vs execution-only). Cheap,
  decides scope for #6.
- **#13** — dump it with Il2CppDumper and build the converter → first real,
  working profile end-to-end with `scry watch` (non-admin).
