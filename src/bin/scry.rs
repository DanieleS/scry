//! `scry` — a command-line host for the telemetry engine.
//!
//! The engine is a library; this is the thin *host* that drives it from a
//! terminal. Its reason to exist is to point the engine at a **real, running
//! game** on the machine you're on — above all on Windows, where the production
//! backend lives — load a profile, let the resolver confirm it actually fits the
//! process, and stream the live values out. No build pipeline, no bespoke host:
//! drop the binary on the box, run it against the game.
//!
//! ```text
//! scry watch --process game.exe --profile game.json
//! scry watch --pid 12345 --profiles ./profiles/       # let the resolver pick
//! scry scan  --process game.exe --signature "48 8B 05 ?? ?? ?? ??"
//! scry selftest                                        # engine smoke test, no game
//! ```
//!
//! It is deliberately dependency-free, mirroring the library: argument parsing
//! and the two platform lookups (find a pid by name, name a pid) are hand-rolled
//! against the same OS surface the backends already use.

// Everything here needs a memory backend for the host OS. On a platform that has
// none (e.g. macOS) the binary still builds, but every command is a no-op that
// says so, rather than failing to compile.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn main() {
    std::process::exit(imp::main());
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn main() {
    eprintln!("scry: no memory backend is built for this platform (Windows and Linux only)");
    std::process::exit(1);
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
mod imp {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use scry::engine::Value;
    use scry::{aob, open_host, resolver, Config, MemoryBackend, Profile, Session};

    const USAGE: &str = "\
scry — read live values out of a running game via a per-game profile.

USAGE:
    scry <command> [options]

COMMANDS:
    watch       Attach to a game and stream its values as they change.
    scan        Find an AOB signature in a running process (profile authoring).
    selftest    Prove the engine end-to-end against a bundled test process.
    help        Show this message.
    version     Print the version.

Run `scry <command> --help` for command-specific options.
";

    const WATCH_USAGE: &str = "\
scry watch — attach to a running game and stream its telemetry.

USAGE:
    scry watch (--process <name> | --pid <n>) (--profile <file>... | --profiles <dir>) [options]

TARGET (one required):
    --process <name>    Attach to the first process with this executable name
                        (e.g. game.exe). Case-insensitive on Windows.
    --pid <n>           Attach to this process id.

PROFILES (at least one source):
    --profile <file>    A profile JSON file. May be given more than once.
    --profiles <dir>    Load every *.json profile in this directory.

    The resolver picks the one profile whose `probe` actually resolves in the
    target's memory — so you can point it at a whole folder of community profiles
    and let the memory decide. If none fits, nothing is read (the fail-safe).

OPTIONS:
    --once              Print one snapshot of all values, then exit.
    --for <secs>        Stop after this many seconds (default: run until killed).
    --tick <ms>         Base polling cadence in milliseconds (default: 50).
    --no-resolve        Skip the probe test and attach the single given profile
                        directly (only valid with exactly one profile).
";

    pub fn main() -> i32 {
        let mut args = std::env::args().skip(1);
        let cmd = match args.next() {
            Some(c) => c,
            None => {
                eprint!("{USAGE}");
                return 1;
            }
        };
        let rest: Vec<String> = args.collect();
        match cmd.as_str() {
            "watch" | "run" => watch(&rest),
            "scan" => scan(&rest),
            "selftest" => selftest(&rest),
            "help" | "-h" | "--help" => {
                print!("{USAGE}");
                0
            }
            "version" | "--version" | "-V" => {
                println!("scry {}", env!("CARGO_PKG_VERSION"));
                0
            }
            other => {
                eprintln!("scry: unknown command '{other}'\n");
                eprint!("{USAGE}");
                1
            }
        }
    }

    // ---- watch: the headline command ------------------------------------------

    fn watch(args: &[String]) -> i32 {
        let mut process: Option<String> = None;
        let mut pid: Option<u32> = None;
        let mut profile_files: Vec<String> = Vec::new();
        let mut profiles_dir: Option<String> = None;
        let mut once = false;
        let mut for_secs: Option<f64> = None;
        let mut tick_ms: u64 = 50;
        let mut no_resolve = false;

        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--process" | "-p" => process = it.next().cloned(),
                "--pid" => match it.next().and_then(|s| s.parse().ok()) {
                    Some(n) => pid = Some(n),
                    None => return usage_err(WATCH_USAGE, "--pid needs a numeric process id"),
                },
                "--profile" => match it.next() {
                    Some(f) => profile_files.push(f.clone()),
                    None => return usage_err(WATCH_USAGE, "--profile needs a path"),
                },
                "--profiles" => profiles_dir = it.next().cloned(),
                "--once" => once = true,
                "--for" => for_secs = it.next().and_then(|s| s.parse().ok()),
                "--tick" => match it.next().and_then(|s| s.parse().ok()) {
                    Some(n) => tick_ms = n,
                    None => return usage_err(WATCH_USAGE, "--tick needs a millisecond count"),
                },
                "--no-resolve" => no_resolve = true,
                "-h" | "--help" => {
                    print!("{WATCH_USAGE}");
                    return 0;
                }
                other => return usage_err(WATCH_USAGE, &format!("unexpected argument '{other}'")),
            }
        }

        // Resolve the target down to a concrete (pid, name) pair.
        let (pid, name) = match (process.as_deref(), pid) {
            (Some(name), _) => match plat::find_pid(name) {
                Some(p) => (p, name.to_string()),
                None => {
                    eprintln!("scry: no running process named '{name}'");
                    return 2;
                }
            },
            (None, Some(p)) => {
                let name = plat::process_name(p).unwrap_or_default();
                (p, name)
            }
            (None, None) => {
                return usage_err(
                    WATCH_USAGE,
                    "a target is required: --process <name> or --pid <n>",
                )
            }
        };

        // Gather the candidate profiles.
        let mut profiles: Vec<Profile> = Vec::new();
        for f in &profile_files {
            match load_profile(Path::new(f)) {
                Ok(p) => profiles.push(p),
                Err(e) => {
                    eprintln!("scry: {f}: {e}");
                    return 1;
                }
            }
        }
        if let Some(dir) = &profiles_dir {
            match load_profiles_dir(Path::new(dir)) {
                Ok(mut ps) => profiles.append(&mut ps),
                Err(e) => {
                    eprintln!("scry: {dir}: {e}");
                    return 1;
                }
            }
        }
        if profiles.is_empty() {
            return usage_err(
                WATCH_USAGE,
                "at least one --profile or --profiles is required",
            );
        }

        // Open the target.
        let backend = match open_host(pid) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("scry: cannot open pid {pid}: {e}");
                eprintln!("      (a game may need this run elevated / as administrator)");
                return 1;
            }
        };

        // Choose the profile. With --no-resolve and a single profile we trust the
        // caller; otherwise the memory decides, exactly as a host would in prod.
        let chosen: &Profile = if no_resolve {
            if profiles.len() != 1 {
                return usage_err(WATCH_USAGE, "--no-resolve needs exactly one --profile");
            }
            &profiles[0]
        } else {
            if name.is_empty() {
                eprintln!(
                    "scry: could not read the executable name for pid {pid}; \
                     pass --process <name>, or --no-resolve with a single profile"
                );
                return 1;
            }
            match resolver::select(&backend, &name, &profiles) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    eprintln!(
                        "scry: no profile fits '{name}' (pid {pid}) — nothing read.\n\
                         This is the fail-safe: a profile's probe must resolve in \
                         the target's memory to claim it."
                    );
                    return 3;
                }
                Err(e) => {
                    eprintln!("scry: resolver failed: {e}");
                    return 1;
                }
            }
        };

        let label = chosen.label.as_deref().unwrap_or("(unlabeled profile)");
        eprintln!("scry: attached to {name} (pid {pid}) with {label}");
        eprintln!(
            "scry: {} watch(es); {}-bit target\n",
            chosen.watches.len(),
            backend.pointer_size() * 8
        );

        let config = Config {
            base_tick: Duration::from_millis(tick_ms.max(1)),
            ..Config::default()
        };
        let mut session = Session::attach(backend, chosen, config);

        let start = Instant::now();

        // First poll always reports every value it can read — the initial picture.
        print_diff(session.poll(Duration::ZERO), start.elapsed());
        if once {
            return 0;
        }

        let deadline = for_secs.map(|s| start + Duration::from_secs_f64(s));
        loop {
            if let Some(d) = deadline {
                if Instant::now() >= d {
                    return 0;
                }
            }
            std::thread::sleep(config.base_tick);
            print_diff(session.poll(start.elapsed()), start.elapsed());
        }
    }

    /// Print each changed label as `+<ms>ms  name = value`, one per line. An empty
    /// diff prints nothing — silence *is* "nothing changed".
    fn print_diff(diff: scry::Snapshot, at: Duration) {
        let ms = at.as_millis();
        for (name, value) in diff {
            println!("+{ms:>7}ms  {name} = {}", fmt_value(&value));
        }
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    fn fmt_value(v: &Value) -> String {
        match v {
            Value::I32(n) => n.to_string(),
            Value::U32(n) => n.to_string(),
            Value::F32(x) => x.to_string(),
            Value::U64(n) => n.to_string(),
            // Quote strings so an empty or space-bearing value is visible.
            Value::Str(s) => format!("{s:?}"),
            // Render a collection as its elements, comma-separated — each element
            // formatted the same way, so a list of strings stays quoted.
            Value::List(items) => {
                let parts: Vec<String> = items.iter().map(fmt_value).collect();
                format!("[{}]", parts.join(", "))
            }
            // Render a record as `{key: value, …}` — each field the same way, so a
            // record nested in a list (a party roster) reads naturally.
            Value::Map(fields) => {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(k, v)| format!("{k}: {}", fmt_value(v)))
                    .collect();
                format!("{{{}}}", parts.join(", "))
            }
            Value::Unavailable => "unavailable".to_string(),
        }
    }

    // ---- scan: signature-finding aid for profile authors ----------------------

    const SCAN_USAGE: &str = "\
scry scan — find an AOB signature in a running process.

USAGE:
    scry scan (--process <name> | --pid <n>) --signature \"<bytes>\"

    Bytes are space-separated hex with `??` wildcards, e.g.:
        scry scan --process game.exe --signature \"48 8B 05 ?? ?? ?? ?? 48 8B 88\"

    Prints the absolute address of the first match, or reports none found.
";

    fn scan(args: &[String]) -> i32 {
        let mut process: Option<String> = None;
        let mut pid: Option<u32> = None;
        let mut sig: Option<String> = None;

        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--process" | "-p" => process = it.next().cloned(),
                "--pid" => pid = it.next().and_then(|s| s.parse().ok()),
                "--signature" | "--sig" => sig = it.next().cloned(),
                "-h" | "--help" => {
                    print!("{SCAN_USAGE}");
                    return 0;
                }
                other => return usage_err(SCAN_USAGE, &format!("unexpected argument '{other}'")),
            }
        }

        let sig = match sig {
            Some(s) => s,
            None => return usage_err(SCAN_USAGE, "--signature is required"),
        };
        let pattern = match aob::parse_pattern(&sig) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("scry: bad signature: {e}");
                return 1;
            }
        };

        let (pid, name) = match (process.as_deref(), pid) {
            (Some(name), _) => match plat::find_pid(name) {
                Some(p) => (p, name.to_string()),
                None => {
                    eprintln!("scry: no running process named '{name}'");
                    return 2;
                }
            },
            (None, Some(p)) => (p, plat::process_name(p).unwrap_or_default()),
            (None, None) => return usage_err(SCAN_USAGE, "a target is required"),
        };

        let backend = match open_host(pid) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("scry: cannot open pid {pid}: {e}");
                return 1;
            }
        };

        match aob::find_in_process(&backend, &pattern) {
            Ok(Some(addr)) => {
                println!("0x{addr:x}");
                0
            }
            Ok(None) => {
                eprintln!("scry: signature not found in {name} (pid {pid})");
                4
            }
            Err(e) => {
                eprintln!("scry: scan failed: {e}");
                1
            }
        }
    }

    // ---- selftest: no game required -------------------------------------------

    const SELFTEST_USAGE: &str = "\
scry selftest — prove the engine end-to-end against the bundled test process.

USAGE:
    scry selftest [--cavia <path>]

    Spawns `cavia` (shipped alongside this binary), attaches to it with the
    platform backend, and checks the full read path — module base, a
    module-relative pointer chain, an AOB scan, a RIP-relative decode, and the
    build/version probe.
    A pass means the memory backend works on this machine.
";

    fn selftest(args: &[String]) -> i32 {
        let mut cavia_override: Option<String> = None;
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--cavia" => cavia_override = it.next().cloned(),
                "-h" | "--help" => {
                    print!("{SELFTEST_USAGE}");
                    return 0;
                }
                other => {
                    return usage_err(SELFTEST_USAGE, &format!("unexpected argument '{other}'"))
                }
            }
        }

        let cavia = match cavia_override {
            Some(p) => PathBuf::from(p),
            None => match sibling_cavia() {
                Some(p) => p,
                None => {
                    eprintln!(
                        "scry: cannot locate the cavia binary next to this executable; \
                               pass --cavia <path>"
                    );
                    return 1;
                }
            },
        };

        let ready = match spawn_cavia(&cavia) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("scry: could not start cavia at {}: {e}", cavia.display());
                return 1;
            }
        };
        // Keep the child alive for the checks; kill on drop.
        let _guard = ready.guard;

        let backend = match open_host(ready.pid) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("scry: cannot open cavia (pid {}): {e}", ready.pid);
                return 1;
            }
        };

        let mut ok = true;
        let mut check = |name: &str, pass: bool, detail: String| {
            println!(
                "  [{}] {name}{}",
                if pass { "PASS" } else { "FAIL" },
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(" — {detail}")
                }
            );
            ok &= pass;
        };

        println!("scry selftest against cavia (pid {})\n", ready.pid);

        // 1. Module base agrees with what the cavia reported about itself.
        match backend.module_base(&ready.exe) {
            Ok(base) => check(
                "module base",
                base == ready.base,
                format!("engine 0x{base:x} vs reported 0x{:x}", ready.base),
            ),
            Err(e) => check("module base", false, e.to_string()),
        }

        // 2. Resolve the Tier-1 pointer chain [PLAYER_offset, hp(0)] and read HP.
        let player_offset = ready.player.wrapping_sub(ready.base) as i64;
        match backend
            .resolve(ready.base, &[player_offset, 0])
            .and_then(|addr| backend.read_i32(addr))
        {
            Ok(hp) => check(
                "pointer chain read",
                hp == ready.hp,
                format!("hp = {hp} (expected {})", ready.hp),
            ),
            Err(e) => check("pointer chain read", false, e.to_string()),
        }

        // 3. AOB scan finds the planted SIG at the address the cavia reported.
        let sig_pattern =
            aob::parse_pattern("53 43 52 59 5A A5 11 22 33 44 55 66 77 88 99 AB").unwrap();
        match aob::find_in_process(&backend, &sig_pattern) {
            Ok(Some(addr)) => check(
                "aob scan",
                addr == ready.sig,
                format!("found at 0x{addr:x} (reported 0x{:x})", ready.sig),
            ),
            Ok(None) => check("aob scan", false, "signature not found".to_string()),
            Err(e) => check("aob scan", false, e.to_string()),
        }

        // 4. RIP-relative decode: the cavia planted a real `mov rax, [rip+disp32]`
        //    whose operand is the PLAYER slot. Decoding it must recover PLAYER's
        //    address — the x64 static-base math a Tier-2 profile relies on.
        match backend.resolve_rip(ready.rip, 3, 7) {
            Ok(addr) => check(
                "rip-relative decode",
                addr == ready.player,
                format!("decoded 0x{addr:x} (player 0x{:x})", ready.player),
            ),
            Err(e) => check("rip-relative decode", false, e.to_string()),
        }

        // 5. Build/version probe: Some(..) on Windows (PE headers), None on Linux
        //    (no PE metadata). Either is a pass; we only prove it doesn't error.
        match backend.module_version(&ready.exe) {
            Ok(v) => check(
                "build id",
                true,
                v.unwrap_or_else(|| "none (expected on Linux)".to_string()),
            ),
            Err(e) => check("build id", false, e.to_string()),
        }

        println!();
        if ok {
            println!("selftest OK");
            0
        } else {
            println!("selftest FAILED");
            1
        }
    }

    // ---- profile loading ------------------------------------------------------

    fn load_profile(path: &Path) -> Result<Profile, String> {
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        Profile::from_json(&text).map_err(|e| e.to_string())
    }

    /// Load every `*.json` in `dir` as a profile. A file that fails to parse is
    /// skipped with a warning rather than sinking the batch — one broken
    /// community profile must not deny telemetry to the valid ones.
    fn load_profiles_dir(dir: &Path) -> Result<Vec<Profile>, String> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match load_profile(&path) {
                Ok(p) => out.push(p),
                Err(e) => eprintln!("scry: skipping {}: {e}", path.display()),
            }
        }
        Ok(out)
    }

    // ---- cavia harness for selftest -------------------------------------------

    struct Ready {
        pid: u32,
        exe: String,
        base: u64,
        player: u64,
        sig: u64,
        rip: u64,
        hp: i32,
        guard: CaviaGuard,
    }

    /// Kills the spawned cavia when the selftest is done.
    struct CaviaGuard(std::process::Child);
    impl Drop for CaviaGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn sibling_cavia() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        let name = if cfg!(windows) { "cavia.exe" } else { "cavia" };
        let path = dir.join(name);
        path.exists().then_some(path)
    }

    fn spawn_cavia(path: &Path) -> Result<Ready, String> {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};

        let mut child = Command::new(path)
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("cavia exited before signalling READY".to_string());
            }
            if line.starts_with("READY ") {
                break;
            }
        }
        parse_ready(&line, child)
    }

    fn parse_ready(line: &str, child: std::process::Child) -> Result<Ready, String> {
        let mut pid = 0u32;
        let mut exe = String::new();
        let (mut base, mut player, mut sig, mut rip, mut hp) = (0u64, 0u64, 0u64, 0u64, 0i32);
        let hex = |s: &str| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok();
        for tok in line.split_whitespace() {
            let (k, v) = match tok.split_once('=') {
                Some(kv) => kv,
                None => continue,
            };
            match k {
                "pid" => pid = v.parse().map_err(|_| "bad pid")?,
                "exe" => exe = v.to_string(),
                "base" => base = hex(v).ok_or("bad base")?,
                "player" => player = hex(v).ok_or("bad player")?,
                "sig" => sig = hex(v).ok_or("bad sig")?,
                "rip" => rip = hex(v).ok_or("bad rip")?,
                "hp" => hp = v.parse().map_err(|_| "bad hp")?,
                _ => {}
            }
        }
        Ok(Ready {
            pid,
            exe,
            base,
            player,
            sig,
            rip,
            hp,
            guard: CaviaGuard(child),
        })
    }

    // ---- small helpers --------------------------------------------------------

    fn usage_err(usage: &str, msg: &str) -> i32 {
        eprintln!("scry: {msg}\n");
        eprint!("{usage}");
        1
    }

    // ---- platform: pid <-> process name --------------------------------------
    //
    // The library takes a pid (a host decides *which* process); these two lookups
    // are the host's job, so they live here, against the same OS surface the
    // backends already use.

    #[cfg(target_os = "linux")]
    mod plat {
        use std::fs;

        /// First pid whose executable basename equals `name`.
        pub fn find_pid(name: &str) -> Option<u32> {
            for entry in fs::read_dir("/proc").ok()? {
                let entry = entry.ok()?;
                let pid: u32 = match entry.file_name().to_str().and_then(|s| s.parse().ok()) {
                    Some(p) => p,
                    None => continue,
                };
                if process_name(pid).as_deref() == Some(name) {
                    return Some(pid);
                }
            }
            None
        }

        /// Executable basename of `pid`, from the `exe` symlink, falling back to
        /// `comm` (which is truncated to 15 bytes, hence the fallback order).
        pub fn process_name(pid: u32) -> Option<String> {
            if let Ok(target) = fs::read_link(format!("/proc/{pid}/exe")) {
                if let Some(name) = target.file_name().and_then(|f| f.to_str()) {
                    return Some(name.to_string());
                }
            }
            fs::read_to_string(format!("/proc/{pid}/comm"))
                .ok()
                .map(|s| s.trim_end().to_string())
                .filter(|s| !s.is_empty())
        }
    }

    #[cfg(target_os = "windows")]
    mod plat {
        use std::ffi::c_void;

        type Handle = *mut c_void;
        const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
        const INVALID_HANDLE_VALUE: isize = -1;
        const MAX_PATH: usize = 260;

        #[repr(C)]
        struct ProcessEntry32W {
            dw_size: u32,
            cnt_usage: u32,
            th32_process_id: u32,
            th32_default_heap_id: usize,
            th32_module_id: u32,
            cnt_threads: u32,
            th32_parent_process_id: u32,
            pc_pri_class_base: i32,
            dw_flags: u32,
            sz_exe_file: [u16; MAX_PATH],
        }

        extern "system" {
            fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> Handle;
            fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
            fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
            fn CloseHandle(handle: Handle) -> i32;
        }

        fn exe_name(entry: &ProcessEntry32W) -> String {
            let end = entry
                .sz_exe_file
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.sz_exe_file.len());
            String::from_utf16_lossy(&entry.sz_exe_file[..end])
        }

        /// Walk the process snapshot, handing each entry to `f`; the first entry
        /// for which `f` returns `Some` short-circuits and is returned.
        fn with_processes<T>(mut f: impl FnMut(&ProcessEntry32W) -> Option<T>) -> Option<T> {
            let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
            if snapshot as isize == INVALID_HANDLE_VALUE || snapshot.is_null() {
                return None;
            }
            let mut entry: ProcessEntry32W = unsafe { std::mem::zeroed() };
            entry.dw_size = std::mem::size_of::<ProcessEntry32W>() as u32;
            let mut result = None;
            if unsafe { Process32FirstW(snapshot, &mut entry) } != 0 {
                loop {
                    if let Some(v) = f(&entry) {
                        result = Some(v);
                        break;
                    }
                    if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
                        break;
                    }
                }
            }
            unsafe { CloseHandle(snapshot) };
            result
        }

        /// First pid whose executable name equals `name`, compared ASCII
        /// case-insensitively as the Windows filesystem does.
        pub fn find_pid(name: &str) -> Option<u32> {
            with_processes(|e| {
                if exe_name(e).eq_ignore_ascii_case(name) {
                    Some(e.th32_process_id)
                } else {
                    None
                }
            })
        }

        /// Executable name of `pid`.
        pub fn process_name(pid: u32) -> Option<String> {
            with_processes(|e| (e.th32_process_id == pid).then(|| exe_name(e)))
        }
    }
}
