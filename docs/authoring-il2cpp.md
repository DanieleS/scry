# Authoring profiles for IL2CPP games (the converter)

This guide covers the **name-based** workflow for a Unity **IL2CPP** game: pin
values by their `Class::field` names and let the offline `il2cpp2scry` converter
derive the numeric offsets.

> This is the automated, IL2CPP-specific companion to
> [`authoring-profiles.md`](authoring-profiles.md), the general "reverse-engineer
> it in Cheat Engine" guide. The manual guide is how you find a chain's **root
> anchor** (a static base or a RIP-relative accessor signature); this guide is how
> you stop hand-counting **field offsets** once you have that anchor. It is the
> first step of the name-based direction in [`DIRECTION.md`](DIRECTION.md).

The idea in one line: **a game patch churns offsets constantly but renames fields
rarely**, so the thing you maintain by hand (names) is the stable thing, and the
brittle thing (offsets) is regenerated. On a patch you re-run the dumper and the
converter with the same map — you don't re-do reverse engineering by hand.

Nothing here touches the runtime. The engine keeps reading plain offsets and
signatures exactly as it always has; all the IL2CPP knowledge lives in this
offline tool, built only under the `authoring` feature.

---

## Why this is read-only and safe

An IL2CPP game ships its own reflection on disk:

- `…_Data/il2cpp_data/Metadata/global-metadata.dat` — the managed metadata, and
- `GameAssembly.dll` — the AOT-compiled code.

[Il2CppDumper] parses those **files** into `class::field → offset` — no injection,
no attaching to a running process, no writing anything. The converter then works
purely on that text plus a small mapping you write. The only step that touches
the live game is the final validation with `scry watch`, which is itself
read-only.

[Il2CppDumper]: https://github.com/Perfare/Il2CppDumper

---

## The workflow

### 1. Dump the game's reflection

Run Il2CppDumper against the two shipped files:

```text
Il2CppDumper.exe GameAssembly.dll global-metadata.dat
```

It writes several artefacts; the one this tool reads is **`dump.cs`**, a
human-readable listing where every field carries its byte offset:

```csharp
// Namespace: Combat
public class PartyMember : MonoBehaviour
{
    // Fields
    public int currentHp; // 0x18
    public int maxHp;     // 0x1C
}
```

> `dump.cs` is the offset source because it is where field offsets actually live.
> Il2CppDumper's `script.json` carries method and metadata *addresses* for IDA /
> Ghidra, but not instance-field offsets — so the converter reads `dump.cs`.

### 2. Find the names of the values you want

Search `dump.cs` for the values you want to read (HP, gold, …) and note their
fully-qualified `Namespace.Class::field` paths, plus any intermediate fields you
must hop through to reach them from a root object. For example, reaching HP might
be `CombatManager::activeParty` (a `PartyMember`) then `PartyMember::currentHp`.

You can drop the namespace (`PartyMember::currentHp`) whenever the class name is
unambiguous across the dump; the converter tells you to qualify it if it isn't.

### 3. Find each chain's root anchor

The converter fills in the *field* offsets, but the **root** of the pointer
chain — where the walk starts — is yours to supply, once, via the manual RE in
[`authoring-profiles.md`](authoring-profiles.md):

- **Tier-2 + `rip`** (recommended on x64): a static field is reached through a
  RIP-relative accessor like `48 8B 05 <disp32>` (`mov rax, [rip+disp32]`). Give
  the wildcarded signature as `anchor` and the decode as a `rip` block
  (`{ "disp": 3, "len": 7 }` for the standard form). The engine recovers the
  static base from the instruction, so only one code signature is fragile across
  patches — not a hardcoded base RVA.
- **Tier-2** (no `rip`): the AOB hit *is* the chain start (the matched bytes are
  the data, or a nearby pointer).
- **Tier-1**: a static, module-relative base. The first chain entry is that
  base's offset, found via a pointer scan.

The converter automates the part a patch breaks (field offsets) and leaves the
part a patch rarely touches (a well-chosen signature) to you.

### 4. Choose an identity probe

Every profile needs a `probe` — the AOB signature the [resolver] scans for to
confirm this profile fits the running process. Two ways to supply it:

- **From a metadata string** (easiest for IL2CPP): a game's `global-metadata.dat`
  is loaded into the process, so a sufficiently unique name in it is a stable,
  scannable identity marker. Give the string and the converter encodes it to
  bytes:

  ```json
  "probe": { "string": "CombatManager" }
  ```

- **From a raw signature** you already found with `scry scan`:

  ```json
  "probe": "48 8B 05 ?? ?? ?? ?? 48 8B 88"
  ```

> **Use a single contiguous token, not a dotted name.** IL2CPP stores a
> namespace and a type name as *separate* entries in its string heap, so
> `"Combat.PartyMember"` never appears as those bytes in a row and a probe for it
> won't resolve. Probe for one identifier — a class name like `"CombatManager"`,
> or better a distinctive **user string literal** the game ships (a scene or
> asset name). Then confirm exactly one hit before trusting it:
>
> ```sh
> scry scan --process SeaOfStars.exe --signature "<the converter's probe bytes>"
> ```
>
> On a first run you can sidestep the probe entirely with `scry watch
> --no-resolve` (see step 7) and pin the identity later.

[resolver]: ../src/resolver.rs

### 5. Write the name map

Put it all together in a `map.json` (full reference below):

```json
{
  "label": "My Game (Steam)",
  "process": "MyGame.exe",
  "module": "GameAssembly.dll",
  "probe": { "string": "Combat.PartyMember" },
  "watches": [
    {
      "name": "hp",
      "tier": "tier2",
      "anchor": "48 8B 05 ?? ?? ?? ?? 48 8B 88",
      "rip": { "disp": 3, "len": 7 },
      "chain": ["CombatManager::activeParty", "PartyMember::currentHp"],
      "type": "i32",
      "rate_hz": 10
    },
    {
      "name": "gold",
      "tier": "tier1",
      "chain": ["0x2C4E120", "InventoryManager::gold"],
      "type": "u32"
    }
  ]
}
```

### 6. Convert

```sh
il2cpp2scry --dump dump.cs --map map.json --out mygame.json
```

The tool reports how many symbols it read and how many watches it resolved, then
writes a normal scry profile with the offsets filled in. It fails loudly — before
you ever attach to the game — on a name the dump doesn't contain, an ambiguous
bare reference, or an unparseable signature.

> `il2cpp2scry` is built only under the `authoring` feature:
> `cargo run --features authoring --bin il2cpp2scry -- --dump … --map …`.

### 7. Validate against the live game

Offsets can only be confirmed against the running game — that step is manual, on
the platform the game runs on (Windows), and is not part of CI:

```sh
scry watch --process MyGame.exe --profile mygame.json
```

Trigger the values in-game (take damage, spend gold) and confirm they change as
expected. If a value reads as `unavailable` or never moves, revisit the chain's
root anchor (step 3) — the field offsets from the dump are the part the tool has
already got right.

### On a game patch

Re-run steps 1 and 6 — dump the patched game, re-run the converter with the
**same** `map.json`. The names carry over; the offsets regenerate. Only if the
patch moved your root anchor (a changed static slot, a broken signature) do you
revisit step 3.

---

## Map format reference

Top-level fields:

| Field | Required | Meaning |
|---|---|---|
| `label` | no | Human-readable label, copied into the profile. |
| `process` | yes | Executable name → `match.process`. |
| `module` | yes | Anchoring module (usually `"GameAssembly.dll"`) → `match.module`, and the default module for Tier-1 watches. |
| `version` | no | Build discriminant → `match.version`. |
| `probe` | yes | Identity signature (see below). |
| `watches` | yes | Array of watches, emitted in order. |

**`probe`** is either a raw signature string, or an object that derives one from
a string:

```json
"probe": "48 8B 05 ?? ?? ?? ??"
"probe": { "string": "CombatManager" }
"probe": { "string": "CombatManager", "encoding": "utf16le" }
```

`encoding` is `utf8` (default) or `utf16le`. The string must be a single
contiguous token present in memory — see the note in step 4.

**Each watch** is one of:

```json
{ "name": "hp", "tier": "tier1", "module": "GameAssembly.dll",
  "chain": [ … ], "type": "i32", "rate_hz": 10 }

{ "name": "gold", "tier": "tier2", "anchor": "48 8B 05 ?? ?? ?? ??",
  "rip": { "disp": 3, "len": 7 }, "chain": [ … ], "type": "u32" }
```

- `module` (Tier-1, optional) defaults to the top-level `module`.
- `anchor` (Tier-2, required) is the AOB signature to scan for.
- `rip` (Tier-2, optional) is a RIP-relative decode `{ "disp", "len" }` applied to
  the anchor before the chain is walked — the x64 static-base shape. Omit it and
  the AOB hit is the chain start. See [`Rip`](../src/profile.rs).
- `type` is one of `i32`, `u32`, `f32`, `u64`, `string`.
- `rate_hz` (optional) is the per-watch sample rate; omit for "every base tick".

A `string` reads an IL2CPP `System.String` referenced at the chain's end (the
chain lands on the reference field; the engine follows the pointer and decodes
length `+0x10` / UTF-16 `+0x14`, capped). A **`collection`** iterates a container
into an ordered array — every chain is name-resolved just like a scalar `chain`:

```json
{ "name": "party_roster", "tier": "collection",
  "base": { "tier": "tier1", "chain": ["0x38BB238", "PlayerPartyManager::currentParty"] },
  "count": ["List`1::_size"], "items": ["List`1::_items"],
  "first": "0x20", "stride": 8,
  "element": ["CharacterDefinitionId::characterId"],
  "type": "string", "max": 16 }
```

- `base` is a nested `{ "tier": …, "chain": […] }` (Tier-1 or Tier-2, same rules
  as a scalar watch) reaching the container.
- `count` / `items` (optional) / `element` are name-resolved chains; `first` and
  `stride` are single literals (or a field reference); `max` caps the count.
- See `docs/authoring-profiles.md` → *Collections* for the runtime semantics.

**`chain`** entries are resolved left to right into the profile's `offsets`. Each
entry is either:

- a **field reference** — a JSON string containing `::`, e.g.
  `"PartyMember::currentHp"` or `"Combat.PartyMember::currentHp"` — replaced by
  that field's offset from the dump; or
- a **literal offset** — a JSON number (`16`) or a numeric string (`"0x18"`,
  `"-4"`) — passed straight through, for the parts a dump can't name (a Tier-1
  static base, a hand-found constant).

The chain follows the engine's pointer-walk semantics: each offset is added and
the result dereferenced, except after the last, where the value is read. See
[`MemoryBackend::resolve`](../src/backend/mod.rs).

---

## Worked example

[`examples/seaofstars/`](../examples/seaofstars/) contains a template map for
**Sea of Stars** (our confirmed IL2CPP target), reading HP and gold, plus a
copy-paste [Windows walkthrough](../examples/seaofstars/walkthrough.md) from a
fresh install to live values.
