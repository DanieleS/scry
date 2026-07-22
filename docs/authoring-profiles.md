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

## Beyond single values

Party/enemy **lists** need iteration — expressed as a data-driven *collection*
watch (base + count + stride + per-element chain), not a scripting engine. See
[#15](https://github.com/DanieleS/scry/issues/15).
