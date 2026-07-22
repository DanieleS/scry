# Sea of Stars — IL2CPP profile template

Sea of Stars is our confirmed Unity **IL2CPP** target. This directory holds a
**template** name map for the `il2cpp2scry` converter, reading **HP** and
**gold**. Follow [`docs/authoring-il2cpp.md`](../../docs/authoring-il2cpp.md) for
the full workflow (and [`docs/authoring-profiles.md`](../../docs/authoring-profiles.md)
for the manual Cheat Engine route it builds on); this README covers only what to
fill in.

`map.json` is a template, not a finished profile: it has placeholders that are
intentionally invalid so a copy-paste run fails fast. Replace them against the
game's own artefacts, then convert.

## What to replace

The map is deliberately explicit about the things only the real game can provide:

1. **Class/field names** (`CombatManager::activeParty`, `PartyMember::currentHp`,
   `InventoryManager::gold`) are *illustrative*. Run Il2CppDumper on the game's
   `GameAssembly.dll` + `global-metadata.dat`, open `dump.cs`, and replace them
   with the real names for the party/combat/inventory objects. The converter
   turns those names into offsets — the part that a game patch churns.

2. **The `hp` Tier-2 anchor** — the `REPLACE_WITH_FOLLOWING_BYTES` in
   `"48 8B 05 ?? ?? ?? ?? …"` — is the tail of the RIP-relative accessor that
   loads the combat manager's static field. Find it in Cheat Engine ("Find out
   what accesses", walk up to the static load), wildcard the 4 displacement
   bytes, and add a few following bytes for uniqueness. The `rip` block
   (`disp: 3, len: 7`) is already the standard `48 8B 05 …` decode.

3. **The `gold` Tier-1 base** — `"0xREPLACE_STATIC_BASE"` — is the
   module-relative offset of the static slot for the gold chain, from a Cheat
   Engine pointer scan. (Or convert this watch to Tier-2 + `rip` too, the
   patch-resilient shape.)

The `probe` string (`"SeaOfStars"`) is a reasonable identity marker — a name the
game's metadata carries — but confirm it's actually present and distinctive for
your build; swap in a more specific class name if not.

## Convert

```sh
il2cpp2scry --dump dump.cs --map examples/seaofstars/map.json --out seaofstars.json
```

(from a checkout: `cargo run --features authoring --bin il2cpp2scry -- …`)

## Validate live (manual, on Windows)

```sh
scry watch --process SeaOfStars.exe --profile seaofstars.json
```

Take damage and spend gold in a battle; confirm `hp` and `gold` change as
expected. Field offsets from the dump are the part the converter has already got
right — if a value never moves, revisit the chain's root anchor (items 2–3).
