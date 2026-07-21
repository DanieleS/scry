//! End-to-end proof of the riskiest mechanic: resolve a real, module-relative
//! pointer chain in a separate process and read the value at the end of it.
//!
//! Spawns the `cavia` binary, parses the anchor facts it prints, then — from
//! the outside, exactly as the host would against a game — derives the static
//! offset, resolves `[base + player_offset] -> deref -> hp` and asserts.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use scry::{LinuxBackend, MemoryBackend};

struct Cavia {
    child: Child,
}

impl Drop for Cavia {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Ready {
    pid: i32,
    exe: String,
    base: u64,
    player: u64,
    hp: i32,
}

fn parse_ready(line: &str) -> Ready {
    let mut r = Ready {
        pid: 0,
        exe: String::new(),
        base: 0,
        player: 0,
        hp: 0,
    };
    for tok in line.split_whitespace() {
        let (k, v) = match tok.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        let hex = |s: &str| u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap();
        match k {
            "pid" => r.pid = v.parse().unwrap(),
            "exe" => r.exe = v.to_string(),
            "base" => r.base = hex(v),
            "player" => r.player = hex(v),
            "hp" => r.hp = v.parse().unwrap(),
            _ => {}
        }
    }
    r
}

fn spawn_cavia() -> (Cavia, Ready) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cavia"))
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn cavia");

    let stdout = child.stdout.take().expect("cavia stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).expect("read cavia line");
        assert!(n > 0, "cavia exited before signalling READY");
        if line.starts_with("READY ") {
            break;
        }
    }
    let ready = parse_ready(&line);
    (Cavia { child }, ready)
}

#[test]
fn resolves_module_relative_pointer_chain() {
    let (_cavia, ready) = spawn_cavia();
    let be = LinuxBackend::new(ready.pid);

    // 1. The engine finds the same module base the target reported.
    let base = be.module_base(&ready.exe).expect("module base");
    assert_eq!(base, ready.base, "module base disagreement");

    // 2. Derive the static offset a profile would store: where PLAYER sits
    //    relative to the module load base.
    let player_offset = (ready.player - base) as i64;

    // 3. Resolve the Tier-1 path exactly as a profile describes it:
    //    start = module base, offsets = [player_offset, hp_field_offset(0)].
    //    That adds player_offset (reaching PLAYER), dereferences (reaching the
    //    heap Stats), then adds the hp field offset without a final deref.
    let hp_addr = be.resolve(base, &[player_offset, 0]).expect("resolve chain");
    let hp = be.read_i32(hp_addr).expect("read hp");

    assert_eq!(hp, ready.hp, "resolved HP mismatch");
    assert_eq!(hp, 1337, "unexpected HP value");
}

#[test]
fn broken_chain_errors_rather_than_lying() {
    let (_cavia, ready) = spawn_cavia();
    let be = LinuxBackend::new(ready.pid);

    // A deliberately bogus first hop: dereferencing an unmapped address must
    // surface as an error, never a garbage value passed off as real.
    let bogus_start = 0xdead_0000_u64;
    let result = be.resolve(bogus_start, &[0, 0]);
    assert!(result.is_err(), "expected a read failure, got {result:?}");
}
