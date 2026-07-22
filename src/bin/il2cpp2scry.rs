//! `il2cpp2scry` — turn Il2CppDumper output into a scry profile, offline.
//!
//! This is an **authoring** tool, not part of the telemetry runtime: it never
//! touches a live process. It reads two files an author already has —
//!
//! - `dump.cs`, Il2CppDumper's read-only dump of the game's IL2CPP reflection
//!   (`Class::field → offset`), and
//! - a small author-written *name map* (`map.json`) that pins each watch to a
//!   dotted `Class::field` path plus a root anchor —
//!
//! and writes a ready-to-use scry profile with every offset resolved. The names
//! are what the author maintains; the brittle numbers are derived, so a game
//! patch means "re-run the dumper and this tool", not "redo the offsets by
//! hand".
//!
//! ```text
//! il2cpp2scry --dump dump.cs --map seaofstars.map.json --out seaofstars.json
//! ```
//!
//! It is built only under the `authoring` feature, so it never lands in a
//! runtime build of `scry`. See `docs/authoring-il2cpp.md` for the full
//! workflow, including the map format.

use std::path::Path;
use std::process::ExitCode;

use scry::authoring::il2cpp;

const USAGE: &str = "\
il2cpp2scry — convert Il2CppDumper output into a scry profile (offline).

USAGE:
    il2cpp2scry --dump <dump.cs> --map <map.json> [--out <profile.json>]

OPTIONS:
    --dump <file>     Il2CppDumper's dump.cs — the field-offset source.
    --map <file>      The author name map (watch -> Class::field paths).
    --out <file>      Write the profile here (default: stdout).
    -h, --help        Show this message.
    -V, --version     Print the version.

The map pins each watch to names; this tool fills in the offsets from the dump.
On a game patch, re-run Il2CppDumper and this tool — the map stays the same.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut dump: Option<String> = None;
    let mut map: Option<String> = None;
    let mut out: Option<String> = None;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dump" => dump = it.next().cloned(),
            "--map" => map = it.next().cloned(),
            "--out" | "-o" => out = it.next().cloned(),
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("il2cpp2scry {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            other => return usage_err(&format!("unexpected argument '{other}'")),
        }
    }

    let dump_path = match dump {
        Some(p) => p,
        None => return usage_err("--dump <dump.cs> is required"),
    };
    let map_path = match map {
        Some(p) => p,
        None => return usage_err("--map <map.json> is required"),
    };

    let dump_src = match std::fs::read_to_string(&dump_path) {
        Ok(s) => s,
        Err(e) => return fail(&format!("cannot read {dump_path}: {e}")),
    };
    let map_src = match std::fs::read_to_string(&map_path) {
        Ok(s) => s,
        Err(e) => return fail(&format!("cannot read {map_path}: {e}")),
    };

    // Parse the dump once so we can report how many symbols it yielded — an
    // empty table almost always means the wrong file was passed.
    let symbols = il2cpp::Symbols::parse(&dump_src);
    if symbols.is_empty() {
        return fail(&format!(
            "no field offsets found in {dump_path} — is it an Il2CppDumper dump.cs?"
        ));
    }

    let spec = match il2cpp::ConvertSpec::from_json(&map_src) {
        Ok(s) => s,
        Err(e) => return fail(&format!("{map_path}: {e}")),
    };

    let profile = match il2cpp::convert(&spec, &symbols) {
        Ok(p) => p,
        Err(e) => return fail(&e.to_string()),
    };

    let json = match profile.to_json() {
        Ok(j) => j,
        Err(e) => return fail(&e.to_string()),
    };

    eprintln!(
        "il2cpp2scry: {} symbols from {dump_path}; {} watch(es) resolved",
        symbols.len(),
        profile.watches.len()
    );

    match &out {
        Some(path) => {
            if let Err(e) = std::fs::write(path, format!("{json}\n")) {
                return fail(&format!("cannot write {path}: {e}"));
            }
            eprintln!("il2cpp2scry: wrote {}", Path::new(path).display());
        }
        None => println!("{json}"),
    }

    ExitCode::SUCCESS
}

fn usage_err(msg: &str) -> ExitCode {
    eprintln!("il2cpp2scry: {msg}\n");
    eprint!("{USAGE}");
    ExitCode::from(2)
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("il2cpp2scry: {msg}");
    ExitCode::FAILURE
}
