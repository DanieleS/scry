# Contracts, profiles and views

How telemetry gets from a game's memory to something a person looks at, and
who owns which part. Decided on 2026-09-24 after a review of scry, Vibepollo and
Ratatoskr kept turning up the same problem from different ends: the thing that
says *which values exist* was tangled up with the thing that says *how to read
them from one build*, and a client's view was keyed on a label scry calls
informational.

## Three things, not one

What is today called a "profile" is really three artefacts with three owners.

| | What it is | Changes when | Knows about clients |
|---|---|---|---|
| **Contract** | The shape of the values: which watches exist, their names and types, what `null` means. Identified by an **id** (`sea-of-stars`) and a **version**. | The data a game offers changes shape. Rarely. | No |
| **Profile** | How to get those values out of one build of one game: offsets, AOB signatures, probes. Declares which contract it implements. | The game patches. Often. | No |
| **View** | How one client draws one contract. Declares the **range** of contract versions it can read. | The client's design changes. | It *is* the client |

Many profiles implement one contract: Steam and GOG builds, each patch. A new
profile for a new build normally keeps the contract, and nothing downstream
changes.

## Who holds what

```
scry-profiles                               ratatoskr-telemetry-views
  contracts/sea-of-stars/2.1.schema.json  ◄── pinned dependency ──  view: contract "sea-of-stars" ^2.0
  profiles/sea-of-stars/steam-1.3.gen.json                          CI → index.json {contract, range, url, sha256}
        │                                                                      │
        ▼ profiles only                                                        ▼ fetched by the client
  Vibepollo ── SSE snapshot {contract:{id,version}, values} ──►  Ratatoskr ──► view
```

- **scry** stays client-agnostic. It reads memory with a profile and says which
  contract the values follow. It knows nothing about hosts, clients or views.
- **`scry-profiles`** (new repository) holds contracts and profiles. Nothing in
  it refers to Vibepollo, Ratatoskr or any other consumer. Kept apart from scry
  because profiles move at the pace of game patches, not of the engine, and a
  corrected offset should not need an engine release.
- **Vibepollo** needs profiles and nothing else. It picks them up from
  `scry-profiles`, lets scry's probes choose the one that fits the running game,
  and forwards the contract identity as it arrives. It never sends, stores or
  knows about views.
- **`ratatoskr-telemetry-views`** (new repository) holds Ratatoskr's views. It
  depends on `scry-profiles` for the contract schemas only, at a pinned version,
  and publishes an index of built views.
- **Ratatoskr** gets its own views. It reads the contract identity from the
  snapshot, looks up a view in the index whose range includes that version,
  downloads it, checks its hash and caches it. The local folder on the device
  stays as a development override.

A view is Ratatoskr's business. Another client could reuse one, but nothing
assumes it can, and the host never carries one.

## The contract identity on the wire

The profile declares both halves:

```json
{ "label": "Sea of Stars (Steam 1.3)",
  "contract": { "id": "sea-of-stars", "version": "2.1" },
  "watches": [ … ] }
```

and scry reports them on `attached`:

```json
{ "event": "attached", "scry": "0.2.0", "profile": "Sea of Stars (Steam 1.3)",
  "contract": { "id": "sea-of-stars", "version": "2.1" }, … }
```

This replaces the integer `contractVersion`. The `label` goes back to being
purely descriptive: renaming it must never unbind a view. A profile with no
contract is still valid for ad-hoc use (`scry watch` at a terminal), but no
client should draw it with a published view.

## Versioning: semver on the contract

- **Minor** (`2.0` → `2.1`): additions only. New watches, new fields inside
  records.
- **Major** (`2.x` → `3.0`): anything else. A rename, a type change, a removal,
  or a value whose meaning changes.

Every value is already nullable (unreadable memory is `null`), so a view that
treats values as optional reads any later minor of its major. A view declares a
range such as `^2.0`.

Compatibility is checked twice. The client does not load a view whose range
excludes the announced version, and there is no unversioned fallback. The view
also checks the version itself at runtime and declines to draw otherwise.

## What CI checks

The schema of each contract version is the single source of truth. It is a
JSON Schema describing the `values` object: one property per watch, its JSON
type, nullable, with optional `description` and `x-unit`. It is not written by
hand: scry knows each watch's type, so `scry schema <profile>` emits the schema
a profile actually produces.

**In `scry-profiles`:**
- Every profile's generated schema is compatible with the contract version it
  claims (every field present, with the declared type). Static; no game needed.
- Each new contract version is diffed against the previous one. A minor that
  removes or retypes a field fails.
- Captured fixtures (`scry watch` recordings) validate against their schema.

**In `ratatoskr-telemetry-views`:**
- TypeScript types are generated from the pinned schema
  (`json-schema-to-typescript`). They replace the hand-written contract types.
- `vue-tsc` in strict mode: using a field that does not exist, or forgetting it
  can be `null`, does not compile.
- A view declaring `^2.0` is type-checked and tested against **every** contract
  version in that range, not only the pinned one. Otherwise the range is only a
  promise.
- Each view is loaded in a headless browser and fed the real fixture, an
  all-`null` picture, a partial one, and extreme values generated from the
  schema. It must not throw or hang.

**What CI cannot check** is meaning. Whether `hp` is current or maximum, or a
duration is in seconds or frames, is in the schema's descriptions and a
reviewer's eyes. And a schema `maximum` does not make memory behave: values read
from a game mid-transition can be anything, so views keep clamping.

## How views reach a client

Decided on 2026-09-24. ratatoskr-telemetry-views deploys `index.json` and the
built views to GitHub Pages from `main`, once every check passes; Pages deploys
the whole site at once, so the index never names a file that is not there yet.
Ratatoskr downloads the index and **only the view for the contract a game
announces**, checks it against its sha256, and keeps both for offline use. A
view pushed onto the device by hand still wins, for trying one before it is
published. A downloaded view is third-party code, which is why the panel's
WebView has no network and no way back into the app.

## Still open

- **How Vibepollo obtains profiles**: bundled with a release, fetched from
  `scry-profiles`, or dropped in by the user as today.

## Where this leaves the current code

- scry: `contractVersion: u32` becomes `contract: { id, version }`, and
  `scry schema` is new.
- Vibepollo: forwards `contract` from `attached` into the snapshot unchanged.
- Ratatoskr: `TelemetryViewStore` keys on contract id and version instead of the
  sanitised label, drops the unversioned fallback, and gains an index-backed
  implementation.
- `telemetry-views/`, currently an untracked folder inside the Ratatoskr
  checkout, becomes `ratatoskr-telemetry-views`. The Sea of Stars profile that
  exists only in its fixtures moves to `scry-profiles` with its contract.
