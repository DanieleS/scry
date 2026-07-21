//! Shared test harness: spawn the `cavia` stand-in game process and parse the
//! anchor facts it prints on its `READY` line. Included by the integration
//! tests via `mod common;`.
//!
//! Each integration-test binary compiles this module independently and uses a
//! different subset of it, so unused-code lints here are expected noise.
#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

/// A spawned cavia process, killed when dropped so no test leaks a child.
pub struct Cavia {
    child: Child,
}

impl Drop for Cavia {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The facts the cavia reports about its own memory, parsed from `READY`.
pub struct Ready {
    pub pid: i32,
    pub exe: String,
    pub base: u64,
    pub player: u64,
    pub sig: u64,
    pub probe: u64,
    pub build: u64,
    pub hp: i32,
}

fn parse_ready(line: &str) -> Ready {
    let mut r = Ready {
        pid: 0,
        exe: String::new(),
        base: 0,
        player: 0,
        sig: 0,
        probe: 0,
        build: 0,
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
            "sig" => r.sig = hex(v),
            "probe" => r.probe = hex(v),
            "build" => r.build = hex(v),
            "hp" => r.hp = v.parse().unwrap(),
            _ => {}
        }
    }
    r
}

/// Spawn the cavia and block until it signals `READY`, returning the process
/// handle (keep it alive for the test's duration) and the parsed facts.
pub fn spawn_cavia() -> (Cavia, Ready) {
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
