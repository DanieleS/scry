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

Because you should expect to re-do a profile per patch, keep the *names* stable
while you do. Add an optional `"contract": { "id": "my-game", "version": "1.0" }`
next to `label`: it names the shape you emit (which watches, of which types), so
a new profile for a new build normally keeps the same contract and nothing
downstream has to change. The id is a lowercase slug and never changes; the
version is `major.minor`. Bump the **minor** when you only *add* a watch or a
record field, and the **major** when you rename, retype or remove one, or when a
value starts to mean something else. scry doesn't read the field — it reports it
on `attached`, for whatever is rendering your values — and `scry schema
<profile.json>` prints the JSON Schema of what the profile emits, so the shape
can be checked rather than remembered.

The older integer `"contractVersion": 1` is still accepted but deprecated. It
reads as version `1.0` with no id, which is enough to say *which* version but not
*of what*, so no renderer can find a view by it. If a profile carries both while
you migrate, they must agree on the major or the profile is rejected.

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

## Records (named fields off one base)

When a value is a handful of **named fields**, not a scalar, use a `record`
watch: resolve one `base`, then read each field as a chain **relative to that
base**. The output is a map (`{ hp: 120, sp: 30 }`).

```json
{ "tier": "record", "name": "player",
  "base": { "tier": "tier1", "module": "GameAssembly.dll", "offsets": ["0x2C4E120", 0] },
  "fields": {
    "hp":   { "offsets": ["0x18"], "type": "i32" },
    "sp":   { "offsets": ["0x1c"], "type": "i32" },
    "name": { "offsets": ["0x38"], "type": { "string": "il2cpp" } }
  } }
```

A field is `{ "offsets": [...], "type": ... }` — the same "walk a chain, read a
type" as a scalar watch, but starting from the record's resolved base. An empty
`offsets` means the base address itself holds the value.

The **same `fields` shape** turns a collection element into a record. Give the
collection `fields` **instead of** `type`; `element` then reaches the element's
base (for a `List<T>` of object pointers, `element: [0, 0]` derefs the slot to
the object), and each field is relative to it:

```json
{ "tier": "collection", "name": "party",
  "base": { "tier": "tier1", "module": "GameAssembly.dll", "offsets": ["0x38BB238", 0] },
  "count": ["0x18"], "items": ["0x10"], "first": "0x20", "stride": 8,
  "element": [0, 0],
  "fields": {
    "name": { "offsets": ["0x38"], "type": { "string": "il2cpp" } },
    "hp":   { "offsets": ["0x18"], "type": "i32" },
    "mp":   { "offsets": ["0x1c"], "type": "i32" }
  }, "max": 8 }
```

Why author a record collection instead of parallel `party_name` / `party_hp`
lists zipped by index? **Coherence** — every field is read off the same element
base in the same tick, so a roster that changes between staggered samples can't
tear (one member's HP under another's name). And **factoring** — the shared
prefix (`element`) is resolved once; each field is a short relative chain.

Rules: a collection sets **exactly one** of `type` (scalar element) or `fields`
(record element) — the profile is rejected at load time otherwise. A broken field
is `unavailable` in place; the record still forms. A base that won't resolve makes
the whole record `unavailable`. Structure is **one level deep** — a field is a
scalar or string, never another record; deeper trees stay the consumer's job.

The IL2CPP converter (`docs/authoring-il2cpp.md`) speaks the same shape with
`Class::field` names in every chain, so the fragile offsets are derived from a
dump rather than hand-counted.

## Derived values (computed, never read)

Games routinely **compute** the numbers a player sees instead of storing them:
percent of max, a party total, an effective stat that is base plus equipment, a
count of living enemies. The arithmetic is trivial; the plumbing to reach the
inputs is not — and without this tier every consumer re-implements it.

A `derived` watch folds values **other watches already produced**, using a small
expression carried as data — the same discipline that makes `collection` express
iteration as `count`/`stride`/`element` rather than as a script:

```json
{ "tier": "derived", "name": "hp_percent", "type": "f32",
  "value": { "mul": [ { "const": 100 },
                      { "div": [ { "watch": "hp" }, { "watch": "hp_max" } ] } ] } }
```

**A derived watch never touches memory.** That one rule is what stops this from
growing into an embedded interpreter, and it makes the failure story trivial: any
unavailable input yields an unavailable output. It is also what makes the tier
**engine-agnostic by construction** — everything engine-specific in scry lives in
offsets, AOB signatures and string layouts, and by the time a value reaches an
expression it is just a number, a string, a list or a map.

| field | meaning |
|---|---|
| `type` | the output type — `i32`, `u32`, `f32`, `u64`. **Never a string**: this is an arithmetic result |
| `each` | *optional*; names a `collection` declared earlier. The expression runs once per element and the watch emits a list |
| `value` | the expression (below) |

### Expression nodes

| node | JSON | meaning |
|---|---|---|
| literal | `{"const": 6}` | a number (hex string accepted, as everywhere else) |
| ref | `{"watch": "hp"}` | that watch's value this tick |
| ref (indexed) | `{"watch": "characters", "index": 2}` | into a list |
| ref (keyed) | `{"watch": "party_progress", "field": "level"}` | into a record |
| ref (both) | `{"watch": "characters", "index": 2, "field": "base_hp"}` | index, then key |
| element | `{"item": "base_patk"}` | a field of the current element; **only under `each`** |
| n-ary | `{"add": […]}` `{"mul": […]}` `{"min": […]}` `{"max": […]}` | at least one operand |
| binary | `{"sub": [a, b]}` `{"div": [a, b]}` | exactly two |
| fold | `{"sum": {"watch": "…", "field": "…", "take": <expr>, "where": [ … ]}}` | Σ over a list watch |
| fold | `{"count": {"watch": "…", "where": [ … ]}}` | how many elements match |
| fold | `{"min": {…}}` `{"max": {…}}` | extremum over a list watch |

`field`, `take` and `where` are each optional on a fold. `field` is required when
the elements are records, omitted when they are scalars; `count` ignores it,
since counting never reads a value.

`min`/`max` deliberately accept **either** form, discriminated by JSON type: an
**array** is n-ary over operands (`{"max": [{"const": 0}, {"watch": "hp"}]}`
clamps at zero), an **object** is a fold over a list
(`{"max": {"watch": "enemies", "field": "hp"}}`). Both are common and the two
JSON types can never collide, so one key carries both.

### Predicates

`where` is a flat list of clauses, **all** of which must hold (implicit AND).
Each compares one field of the element against a literal:

```json
"where": [ { "field": "kind", "eq": "PlayerAddStatModifier" },
           { "field": "stat", "eq": 0 } ]
```

`eq`, `ne`, `lt`, `le`, `gt`, `ge`. Numbers compare numerically; strings only
under `eq`/`ne` (an ordering on text would be a collation policy the engine has
no business having). Clauses stay flat — no nesting, no boolean algebra, no `or`.
A profile that genuinely needs `or` is a signal to reconsider the profile, not to
extend the language.

Filtering is not decoration: **heterogeneous lists are the norm.** `sum hp where
alive == 1`, `count where team == 2`, `sum damage where school == "fire"`. In the
Sea of Stars profile the gear-modifier list is polymorphic, and one element is a
`PercentageBasicAttackDamageHealModifier` whose `stat` field reads `1041865114` —
adjacent memory reinterpreted. Without a clause on the type tag the sum silently
produces a garbage number. A C++ game would tag with a vtable/RTTI name, an ECS
with an integer component id; the predicate is the same either way, and reaching
the discriminant is the profile's job exactly as it is for every other field.

### Fail-soft rules

- The expression evaluates in `f64`, then coerces to `type`. Integer types
  truncate toward zero. A non-finite result, or one outside the target's range,
  is `unavailable` — **never a saturated number**, which would be a lie a
  consumer can't detect.
- **Any** referenced watch that is missing, `unavailable`, or the wrong shape
  (indexing a scalar, keying a list, a field that isn't a number where arithmetic
  needs one) makes the whole expression `unavailable`.
- Index out of range → `unavailable`. Division by zero → `unavailable`.
- `take` clamps to `[0, len]`; a negative or unavailable `take` is `unavailable`.
- `sum` over an empty or fully-filtered list is `0` — "nothing matched" is a real
  answer. But an `unavailable` element *inside* the summed range makes the sum
  `unavailable`: skipping it would quietly under-report. An empty `min`/`max`
  fold **is** `unavailable` — there is no neutral element to return honestly.
- A `where` clause naming a field the element lacks makes the fold
  `unavailable`, rather than silently filtering everything out.
- Under `each`, an element whose expression fails is an `unavailable` in place
  and the list still forms — exactly how `collection` already treats elements.

### Ordering, and why there is no dependency graph

Every `{"watch": n}` must name a watch declared **earlier in the array**. That is
the whole cycle-prevention story: a cycle is unrepresentable rather than merely
detected, there is no graph to walk and no topological sort to run, and
evaluation order simply falls out of declaration order. A forward or self
reference is rejected at load time, naming both watches.

Each tick runs in two phases: every due *memory* watch is sampled first, then
every due *derived* watch is evaluated against the result. So a derived watch
sees fresh values for whatever was sampled this tick and the most recent value
for whatever wasn't due — the correct "latest known" semantics. Derived watches
also never count toward the failure streak that triggers a re-attach: they read
no memory, so letting them vote would make a formula error look like a detached
process.

### Known limitation

A collection element's fields must be scalars, so there is no way to hang a
per-entity sub-list off an element. That is why a profile computing, say, each
character's max HP from their own level-up and upgrade lists has to declare one
collection per character (`garl_levelups`, `valere_levelups`, …) instead of one
`each` over the roster. Letting a collection field be itself a collection,
recursively with a depth cap, would collapse those — but it is a real relaxation
of the "one shallow level of structure" doctrine above, so it is a separate
decision, not a corner of this one.
