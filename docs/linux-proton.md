# Reading a game on Linux, under Proton

Where scry, and the Vibepollo integration around it, stand if the gaming PC moves to a Linux
distribution such as CachyOS. Written on 2026-09-24 against scry 0.1.0-alpha.4 and Vibepollo
2.0.0-beta.3.4. Nothing here has been tried against a real Proton game yet; each point says what
the code does today and what is inferred.

## What already works

- **Clients.** Ratatoskr and its views only talk to Vibepollo's `/telemetry` stream and do not care
  which OS the host runs.
- **Developing views.** Node, Vite and Playwright run the same on Linux.
- **Developing scry.** Linux is the easier platform for it: the backend (`process_vm_readv` over
  `/proc`) exists and the integration tests run there, while on macOS they do not compile.
- **Vibepollo as a streaming host.** It already supports Linux (kmsgrab, kwingrab, gamescope), and
  its CI builds an Arch package.

## What does not work yet

### 1. Vibepollo's scry integration is Windows-only

`src/platform/windows/scry_integration.cpp` implements the supervisor, and the `/telemetry`
endpoint in `nvhttp.cpp` is under `#ifdef _WIN32`. Most of that code is portable: the supervisor
loop, event parsing, the snapshot and subscriptions, the SSE stream and the permission checks.
What is Windows-specific:

- **Spawning the helper.** `CreateProcess` with a kill-on-close job object and inherited pipes.
  On Linux: `posix_spawn` or `fork`/`exec`, with `PR_SET_PDEATHSIG` so the helper dies with the
  host.
- **Choosing the target.** `resolve_target()` relies on `foreground_app`, the Windows foreground
  window confirmed to belong to the running app. On Linux the game would have to be found another
  way: as a descendant of the process Vibepollo launched (usually Steam's reaper), or through
  gamescope or KWin. What Vibepollo's Linux side already exposes for this has not been checked.
- **The helper path.** `tools\scry.exe` next to `sunshine.exe`.

The work is roughly one new file under `src/platform/linux/` plus lifting the `#ifdef` from the
endpoint.

### 2. scry cannot yet identify a Wine process

Under Proton the game is a Windows executable running inside a Wine process.

- **Reading memory should work.** Wine maps `GameAssembly.dll` from its file, so
  `LinuxBackend::module_base` should find it in `/proc/<pid>/maps` by file name, and the offsets are
  the same because the binary is the same. Inferred, not tried.
- **The process name does not match.** `process_name()` in `src/bin/scry.rs` prefers the
  `/proc/<pid>/exe` link. Under Wine that points at `wine64-preloader` (or `wine64`), not at
  `SeaOfStars.exe`, so no profile's `match.process` fits and scry exits with `no_profile_fits`.
  Falling back to `/proc/<pid>/comm` is not enough either: it is cut at 15 bytes, which longer
  executable names exceed. The fix is to take the basename of `argv[0]` from `/proc/<pid>/cmdline`
  when the executable is Wine's loader; Wine puts the Windows path of the game there. Small.
- **No build version on Linux.** The version probe reads the PE header only on Windows and returns
  `None` on Linux (`scry selftest` reports it as "none (expected on Linux)"), so a profile that pins `match.version`
  is never confirmed. `src/backend/pe.rs` already parses PE headers; reading the header of the
  mapped module through the Linux backend would close this. Small to medium.
- **ptrace permission.** Arch and CachyOS ship Yama with `ptrace_scope=1`: a process may only read
  its own descendants. The game is started by Steam, not by Vibepollo, so scry needs
  `CAP_SYS_PTRACE` (`setcap cap_sys_ptrace+ep` on the binary), set by Vibepollo's package.

### 3. Game metadata is thinner

- **Playnite** is Windows-only, so on Linux metadata comes from IGDB alone.
- **IGDB art is not saved on Linux.** Converting it to PNG uses
  `platf::img::convert_to_png_96dpi`, which exists only on Windows, so a Linux host serves IGDB's
  text and no covers or backgrounds.

## Suggested order

1. Fix the two small scry gaps first: the process name under Wine, and the PE version read
   through the Linux backend. They are cheap, and they are what would block the first attempt.
2. On the Linux machine, run `scry watch --pid <game> --profiles <dir>` by hand against Sea of
   Stars under Proton. If it attaches and the values look right, the reading side is proven.
3. Port the supervisor to `src/platform/linux/` in Vibepollo, choosing the target from the
   launched process tree, and ship scry with `CAP_SYS_PTRACE`.
4. Add a PNG conversion for IGDB art on Linux.
