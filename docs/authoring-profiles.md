# Authoring a scry profile from a real game

How to go from a running game to a working scry profile, using Cheat Engine (or
any memory tool) for the one-time reverse engineering. This captures the
workflow we worked out against **Sea of Stars**; it generalises to any target.

Authoring is **offline and one-time** — inject, dump, whatever; it is *not* the
runtime. The runtime (`scry watch`) only ever reads. Do the RE however is
convenient, then bake the result into a JSON profile.

## 0. Attach without admin

A same-user, single-player Steam game is readable without elevation. To prove it
and to run CE non-elevated:

```bat
:: a normal (non-admin) command prompt
set __COMPAT_LAYER=RUNASINVOKER
start "" "C:\Program Files\Cheat Engine 7.5\cheatengine-x86_64.exe"
```

Verify in Task Manager → Details → *Elevated* column = **No**. If `Open Process`
then works on the game, so does scry (same user-mode `OpenProcess` +
`ReadProcessMemory`). Admin is only needed for elevated / other-user / protected
(anti-cheat) targets — not these.

## 1. Find the value (dynamic address)

1. `Open Process` → the game. Enter a state where the value is visible and
   changeable (e.g. a battle, for HP).
2. Value Type `4 Bytes` (Unity ints; try `Float` if it doesn't converge),
   `Exact Value`, scan the current number → **First Scan**.
3. Change the value in-game → **Next Scan** with the new number. Repeat until a
   few addresses remain; confirm the right one by watching it track the game.

This address is **dynamic** (changes every run) — expected.

## 2. Get the field offset

Right-click the address → **Find out what accesses this address** → trigger a
change in-game. You'll see instructions like `mov [rbx+58],edx`:

- `+58` (**hex**, i.e. `0x58`) is the **field offset**. Note it.
- `rbx` holds the **object pointer** — the next link up.

Double-click an instruction → **More information** → read the base register's
value (the object's address).

## 3. Reach a static anchor

You need to reach an address that is **static** — in Cheat Engine, shown
**green** and as `GameAssembly.dll+XXXXX` (module + offset), because it lives at
a fixed offset in a loaded module and survives restarts. A bare hex address
(`1F2A3B4C0`) is dynamic.

Two routes:

### Route A — Pointer scan (→ Tier-1, quick)
Right-click the value → **Pointer scan for this address** → keep the defaults
(`Scan for address`, Max level ~7, Maximum offset value 4095) → **OK**, save the
`.PTR`. Then **restart the game, re-find the value, and Rescan memory** with the
new address to keep only stable paths. Repeat once or twice. A surviving chain
`GameAssembly.dll+BASE → off → … → 0x58` becomes a **Tier-1** watch.

### Route B — Accessor + RIP-relative (→ Tier-2 + `rip`, patch-resilient)
Walk up until a link lives in a static field. You'll find the accessor as a
RIP-relative load — CE annotates the operand as `[GameAssembly.dll+XXXX]`:

```
48 8B 05 xx xx xx xx    mov rax,[GameAssembly.dll+OFFSET]   ; [rip+disp32]
```

Take its bytes, **wildcard the 4 displacement bytes**, add a few following bytes
for uniqueness → the `anchor`. Measure from the instruction start:
`disp = 3`, `len = 7` for the standard `48 8B 05 …` / `48 8D 05 …` forms. Count
the bytes on screen if it's a longer/prefix-less form.

## 4. Write the profile

Offsets accept **hex strings or decimal**, mixed freely — paste what CE shows.

**Tier-1 (pointer scan):**
```json
{ "tier": "tier1", "name": "hp", "module": "GameAssembly.dll",
  "offsets": ["0x1A2B3C", "0x20", "0x58"], "type": "i32" }
```

**Tier-2 + RIP-relative (accessor):**
```json
{ "tier": "tier2", "name": "hp",
  "anchor": "48 8B 05 ?? ?? ?? ?? <a few following bytes>",
  "rip": { "disp": 3, "len": 7 },
  "offsets": ["0x0", "0x58"], "type": "i32" }
```

The RIP decode computes `base = anchor + len + i32_at(anchor + disp)`, then
`offsets` walk from there (each dereferenced except the last).

For the mandatory `match.probe`, use any stable signature in the module. For a
first run you can skip the probe test with `--no-resolve`.

## 5. Verify with scry

```sh
scry scan  --process SeaOfStars.exe --signature "48 8B 05 ?? ?? ?? ?? …"   # exactly one hit
scry watch --process SeaOfStars.exe --profile seaofstars.json --no-resolve # HP changes in battle
```

## Stability, honestly

- **Tier-1** pins the static base RVA *and* every offset → breaks on nearly
  every game update. Fine for validating now; expect to redo it per patch.
- **Tier-2 + `rip`** reduces the fragile surface to one code signature (the base
  RVA is recovered from the instruction) — survives many minor patches. Field
  offsets still break if data structures are reordered.
- The durable answer is **name-based resolution** from engine metadata
  (`docs/DIRECTION.md`), where offsets are re-derived per build. For a
  single-player game you can also just **pin the game version** (disable
  auto-update) so nothing moves until you choose.

## Strings

A `string` value type reads text. How a string is laid out varies **by engine**
(IL2CPP, Mono, native C, Unreal `FString`, …), so the layout is *data*, not baked
into the runtime — you name a **preset** or give an explicit **layout**. No engine
is the default; a bare `"string"` is rejected.

```json
{ "tier": "tier1", "name": "hero",
  "offsets": ["0x1A2B3C", "0x38"], "type": { "string": "il2cpp" } }
```

The chain's last offset lands on the field (Sea of Stars: the character's
`CharacterDefinitionId` string at `+0x38`); the string type follows the reference
itself, so don't add a trailing deref.

**Presets** (validated engine layouts — peers, added as we confirm them):

| preset | layout it stands for |
|---|---|
| `il2cpp` | reference → object; UTF-16, 32-bit length at `+0x10`, chars at `+0x14` |

**Explicit layout** — the escape hatch for any engine without a preset:

| field | meaning |
|---|---|
| `encoding` | `utf8` (1 byte/unit) or `utf16` (2 LE bytes/unit) — required |
| `len_at` | offset to a 32-bit length prefix; **omit for NUL-terminated** (native/C strings) |
| `chars_at` | offset to the first code unit; default `0` |
| `deref` | `true` if the resolved address holds a *pointer* to the object (managed reference types); default `false` (an inline/native buffer) |

```jsonc
// what "il2cpp" expands to, written by hand
"type": { "string": { "encoding": "utf16", "len_at": "0x10", "chars_at": "0x14", "deref": true } }

// a native NUL-terminated UTF-8 C string, read in place
"type": { "string": { "encoding": "utf8" } }
```

The read length is capped (1 KiB) so a garbage length or a missing terminator
can't run away; a null reference reads as `""` (honest empty), not `unavailable`.

## Collections (lists & arrays)

Party/enemy **lists** need iteration. Rather than a scripting engine, a
`collection` watch expresses it as **data** — a base chain to the container, a
`count`, a `stride`, and a per-element chain — and emits an ordered array that
diffs like any other value. It stays structurally read-only and zero-dependency.

Fields:

| field | meaning |
|---|---|
| `base` | how to reach the container — a nested `{ "tier": "tier1"/"tier2", … }`, same shapes as a scalar watch, whose `offsets` end at the list object / array |
| `count` | chain from the container to the 32-bit element count (clamped to `max`) |
| `items` | *optional* chain to the backing-array **pointer** (dereferenced); omit it when the elements live at the container itself (a bare pointer array) |
| `first` | byte offset to element 0 within the element region (an array header); default `0` |
| `stride` | bytes between consecutive elements (a pointer array → `8`) |
| `element` | per-element chain from a slot to the value; empty means the slot *is* the value's address |
| `type` | element type (`i32` … or a `string` — see [Strings](#strings)) |
| `max` | hard cap — a garbage count can neither allocate nor loop unboundedly |

The C# `List<T>` shape (validated against Sea of Stars — `items` at `+0x10`,
`count` at `+0x18`, array header `0x20`, pointer stride `8`) reading the party
roster as an ordered list of names:

```json
{ "tier": "collection", "name": "party_roster",
  "base": { "tier": "tier1", "module": "GameAssembly.dll",
            "offsets": ["0x38BB238", 0] },
  "count": ["0x18"], "items": ["0x10"], "first": "0x20", "stride": 8,
  "element": [], "type": { "string": "il2cpp" }, "max": 16 }
```

A bare pointer array (an "enemy HP list" of entity pointers), with the count read
from the container and each element's HP reached through the entity:

```json
{ "tier": "collection", "name": "enemy_hp",
  "base": { "tier": "tier2", "anchor": "48 8B 05 ?? ?? ?? ?? …",
            "rip": { "disp": 3, "len": 7 }, "offsets": [0] },
  "count": [16], "stride": 8, "element": ["0x0", "0x58"], "type": "i32", "max": 64 }
```

Per-element resolution is **fail-soft**: a broken element is `unavailable` in
place without sinking the list. A base/count/items failure makes the whole watch
`unavailable` — the list can't be sized or located, so there is nothing honest to
emit.

The IL2CPP converter (`docs/authoring-il2cpp.md`) speaks the same shape with
`Class::field` names in every chain, so the fragile offsets are derived from a
dump rather than hand-counted.
