# Release notes

One Markdown file per release, named after the version, **committed before the
tag** — the release workflow reads it out of the tagged commit itself, so notes
added afterwards do not count.

Their presence is the release gate. A tag with no matching notes file is not a
release, it is just a tag: CI runs as normal and publishes nothing. That is
deliberate — it means an accidental or exploratory tag can never ship, and every
release is something somebody sat down and described.

## Cutting a release

1. Bump `version` in `Cargo.toml` (and refresh `Cargo.lock` — build once). The
   workflow fails the release if the crate and the tag disagree, because
   `scry --version` has to match the tag people downloaded it under.
2. Write `release_notes/<version>.md`.
3. Commit both, tag, and push them together:

   ```sh
   git tag -a v0.1.0 -m "scry 0.1.0"
   git push --follow-tags
   ```

   The tag must be **annotated** (`-a`): `--follow-tags` pushes only those, and
   a lightweight tag would silently stay on your machine while the commit went
   out — leaving a push that looks fine and releases nothing.

The tag must sit on the commit being pushed to `main`. CI notices it, checks
that it is a version tag, that it is newer than what is already published in its
channel, that no release exists for it yet, and that these notes are present —
then builds and publishes. If any of that does not hold, the run is an ordinary
CI run.

To re-run a release that failed part-way, use the **CI** workflow's
`workflow_dispatch` with `release_tag: v0.1.0`. Publishing is idempotent: it
updates the existing release and replaces its assets rather than duplicating.

## Naming

`release_notes/0.1.0.md` for tag `v0.1.0` (the `v` is optional on the tag; the
notes filename never has it). A suffix picks the channel and decides whether the
release is marked as a pre-release and whether it becomes "latest":

| Tag | Channel |
|---|---|
| `v0.1.0` | stable — becomes the latest release |
| `v0.2.0-rc.1`, `v0.2.0-beta.1`, `v0.2.0-alpha.1` | pre-release |

## What gets published

| Asset | For |
|---|---|
| `scry-<version>-x86_64-pc-windows-msvc.zip` | `scry.exe` — what a host application vendors |
| `scry-<version>-i686-pc-windows-msvc.zip` | the same, 32-bit |
| `scry-tools-<version>-x86_64-pc-windows-msvc.zip` | `il2cpp2scry.exe` + `cavia.exe` — profile authoring |
| `SHA256SUMS` | pin the download by hash |

Profiles are **not** in here. They have their own cadence — a game patch means a
new profile, not a new engine — and tying them to an engine version would force
a release every time a game updates.
