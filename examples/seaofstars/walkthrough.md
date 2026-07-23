# Sea of Stars — end-to-end Windows walkthrough

A copy-paste checklist to go from a fresh Windows box with Sea of Stars to
`scry watch` printing live HP and gold. This is the manual, one-time work the
converter can't do for you (finding the anchor and the real field names); the
converter does the rest.

Everything here is **read-only** and runs as your normal user — no admin.

---

## 0. Prerequisites

- **Sea of Stars** installed (Steam). It's a 64-bit Unity **IL2CPP** build.
- **scry.exe**, **cavia.exe**, **il2cpp2scry.exe** — all three ship as one
  artifact. Download `scry-x86_64-pc-windows-msvc` from any green CI run and
  unzip; **no Rust toolchain needed**. (`scry.exe` is the host, `cavia.exe` the
  selftest target, `il2cpp2scry.exe` the IL2CPP→profile converter.) To build
  instead: `cargo build --release --bins --features authoring`.
- **[Il2CppDumper]** — download the latest release zip, extract.
- **[Cheat Engine] 7.5** — for the one-time reverse engineering.

[Il2CppDumper]: https://github.com/Perfare/Il2CppDumper/releases
[Cheat Engine]: https://cheatengine.org/

Find the install folder (Steam → right-click Sea of Stars → Manage → Browse local
files). You'll see, roughly:

```
Sea of Stars\
  SeaOfStars.exe
  GameAssembly.dll
  SeaOfStars_Data\
    il2cpp_data\Metadata\global-metadata.dat
```

## 1. Prove the backend works (no game logic yet)

Drop `scry.exe` and `cavia.exe` in the same folder and run:

```bat
scry.exe selftest
```

Expect `selftest OK`. That confirms `ReadProcessMemory`, module base, the pointer
chain, the AOB scan and the RIP-relative decode all work on your machine.

## 2. Dump the reflection

```bat
Il2CppDumper.exe "…\Sea of Stars\GameAssembly.dll" ^
                 "…\Sea of Stars\SeaOfStars_Data\il2cpp_data\Metadata\global-metadata.dat" ^
                 out
```

Open `out\dump.cs`. This is your field-offset source. Keep it open — you'll
search it in step 5.

## 3. Launch Cheat Engine non-elevated

```bat
:: a normal (non-admin) command prompt
set __COMPAT_LAYER=RUNASINVOKER
start "" "C:\Program Files\Cheat Engine 7.5\cheatengine-x86_64.exe"
```

Task Manager → Details → *Elevated* = **No**. `Open Process` → `SeaOfStars.exe`.
If CE can attach non-elevated, so can scry.

## 4. Find HP, then its anchor (do gold the same way)

Start a **battle** so HP is visible and changeable.

1. **Dynamic address.** Value Type `4 Bytes`, `Exact Value` = current HP →
   *First Scan*. Take damage / heal → *Next Scan* with the new number. Repeat to
   a handful of addresses; keep the one that tracks HP.
2. **Field offset + object.** Right-click → *Find out what accesses this address*
   → take a hit in battle. You'll see e.g. `mov [rbx+58],edx`: `0x58` is the
   field offset, `rbx` is the object pointer (the next link up).
3. **Reach a static anchor** — pick one route:
   - **Route B (recommended, Tier-2 + `rip`):** walk up until a link is loaded
     from a static field via `48 8B 05 xx xx xx xx  mov rax,[GameAssembly.dll+…]`.
     Wildcard the 4 displacement bytes and add a few following bytes:
     `anchor = "48 8B 05 ?? ?? ?? ?? <next bytes>"`, `rip = { disp: 3, len: 7 }`.
   - **Route A (quick, Tier-1):** *Pointer scan for this address*, restart the
     game once and *Rescan* to keep only stable paths; a surviving
     `GameAssembly.dll+BASE → … → 0x58` becomes the Tier-1 `chain`.

See [`../../docs/authoring-profiles.md`](../../docs/authoring-profiles.md) for the
detailed CE steps and screenshots-in-prose.

## 5. Name the fields in `dump.cs`

For each offset you found (`0x58`, the intermediate ones), find the class and
field in `dump.cs` that matches — e.g. searching for the value's class turns
`0x58` into `PartyMember::currentHp`. Note the `Namespace.Class::field` path for
each hop. These names are what you put in the map; the converter turns them back
into the offsets, so next patch you just re-dump.

## 6. Fill in the map and convert

Edit [`map.json`](map.json): replace the placeholder names with the real ones,
the `hp` anchor with your signature, and (for Tier-1) the gold base. For the
`probe`, pick **one** distinctive token — a class name, or a unique string
literal from `dump.cs` — not a dotted `Namespace.Type`.

```bat
il2cpp2scry.exe --dump out\dump.cs --map map.json --out seaofstars.json
```

## 7. Watch it live

```bat
:: first run: skip the probe test while you validate the offsets
scry.exe watch --process SeaOfStars.exe --profile seaofstars.json --no-resolve

:: confirm your probe resolves (want exactly one hit), then drop --no-resolve
scry.exe scan  --process SeaOfStars.exe --signature "<probe bytes from seaofstars.json>"
scry.exe watch --process SeaOfStars.exe --profile seaofstars.json
```

Take damage and spend gold; `hp` and `gold` should print as they change. If a
value reads `unavailable` or never moves, the field offsets are almost certainly
right (they came from the dump) — recheck the anchor (step 4).
