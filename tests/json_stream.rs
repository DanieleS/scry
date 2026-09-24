//! End-to-end proof of the **JSON Lines contract** — the shape a host program
//! consuming `scry` as a subprocess is entitled to.
//!
//! Every other test drives the engine as a library. This one drives the *binary*
//! the way a host does — spawn it against a real process, read its stdout, parse
//! it — because the contract being tested is the process boundary itself: one
//! JSON object per line, flushed per tick, diagnostics kept off stdout.
//!
//! Only compiled where the CLI has a memory backend; elsewhere `scry` is a stub
//! that refuses to run, and there is no stream to assert on.
#![cfg(any(target_os = "linux", target_os = "windows"))]

use std::path::PathBuf;
use std::process::Command;

mod common;
use common::spawn_cavia;

const PROBE_SIG: &str = "50 52 4F 42 45 5F A5 5A 01 23 45 67 89 AB CD EF";

/// `frame` sits at offset 8 within `Stats` — the cavia bumps it on a timer, so
/// it is the value that proves the stream carries *changes*, not just the first
/// picture. `hp` at offset 0 never moves, and so must fall silent.
const FRAME_FIELD: i64 = 8;
const HP_FIELD: i64 = 0;

/// A scratch dir for the profile file, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("scry-json-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Scratch(dir)
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).expect("write scratch file");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The two-watch profile over the cavia, as JSON — written to disk because the
/// point is to exercise the binary's own loading path, not to hand it a struct.
fn cavia_profile_json(exe: &str, player_offset: i64) -> String {
    format!(
        r#"{{
          "label": "cavia",
          "contract": {{ "id": "cavia", "version": "3.1" }},
          "match": {{
            "process": "{exe}",
            "module": "{exe}",
            "probe": "{PROBE_SIG}"
          }},
          "watches": [
            {{ "tier": "tier1", "name": "frame", "module": "{exe}",
               "offsets": [{player_offset}, {FRAME_FIELD}], "type": "i32" }},
            {{ "tier": "tier1", "name": "hp", "module": "{exe}",
               "offsets": [{player_offset}, {HP_FIELD}], "type": "i32" }}
          ]
        }}"#
    )
}

/// Run `scry watch --format json` against the cavia for a bounded window and
/// return its stdout lines, parsed. Bounded by `--for` so the test cannot hang.
fn watch_json() -> Vec<serde_json::Value> {
    let (_cavia, ready) = spawn_cavia();
    let scratch = Scratch::new();
    let profile = scratch.write(
        "cavia.json",
        &cavia_profile_json(&ready.exe, (ready.player - ready.base) as i64),
    );

    let out = Command::new(env!("CARGO_BIN_EXE_scry"))
        .args(["watch", "--pid"])
        .arg(ready.pid.to_string())
        .arg("--profile")
        .arg(&profile)
        .args([
            "--no-resolve",
            "--format",
            "json",
            "--tick",
            "20",
            "--for",
            "1",
        ])
        .output()
        .expect("run scry watch");

    assert!(
        out.status.success(),
        "scry watch failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("stdout must be one JSON object per line: {l:?} — {e}"))
        })
        .collect()
}

/// The stream opens by saying what it attached to, carries values, and closes
/// deliberately. A host reads exactly this and nothing else on stdout.
#[test]
fn stdout_is_a_stream_of_json_events() {
    let events = watch_json();
    assert!(
        events.len() >= 3,
        "expected attached + at least one values + detached, got {events:#?}"
    );

    let attached = &events[0];
    assert_eq!(attached["event"], "attached");
    assert_eq!(attached["profile"], "cavia");
    assert_eq!(attached["watches"], 2);
    assert!(
        attached["pid"].is_u64() && attached["pointer_bits"].is_u64(),
        "attach identity must be machine-readable: {attached}"
    );
    // Which profile won, and what shape it emits: the host can't derive either
    // on its own, so both travel with the attach. The contract is passed through
    // verbatim from the file — 3.1 here precisely because nothing defaults to it.
    assert_eq!(
        attached["contract"],
        serde_json::json!({ "id": "cavia", "version": "3.1" })
    );
    // The deprecated integer still carries the major, for hosts that read only it.
    assert_eq!(attached["contract_version"], 3);
    assert!(
        attached["profile_file"]
            .as_str()
            .is_some_and(|p| p.ends_with("cavia.json")),
        "attach must name the file the profile came from: {attached}"
    );

    let last = events.last().unwrap();
    assert_eq!(last["event"], "detached");
    assert_eq!(last["reason"], "duration", "`--for` ran out: {last}");

    // Every event is self-describing and time-stamped past the attach line, so a
    // host never has to infer what a line is from its position in the stream.
    for event in &events[1..] {
        assert!(event["event"].is_string(), "untagged event: {event}");
        assert!(event["t_ms"].is_u64(), "event without a timestamp: {event}");
    }
}

/// The first `values` event is the whole readable picture; later ones carry only
/// what moved. `frame` ticks, `hp` never does — so `hp` must appear once, then
/// never again, which is what makes the stream cheap enough to poll at 50 Hz.
#[test]
fn values_events_carry_the_first_picture_then_only_changes() {
    let events = watch_json();
    let values: Vec<&serde_json::Value> = events
        .iter()
        .filter(|e| e["event"] == "values")
        .map(|e| &e["values"])
        .collect();

    assert!(
        values.len() >= 2,
        "the cavia bumps `frame` on a timer, so the stream must show it change: {events:#?}"
    );

    let first = values[0];
    assert!(
        first["hp"].is_i64() && first["frame"].is_i64(),
        "the first picture must carry every readable watch, untagged: {first}"
    );

    for later in &values[1..] {
        // Absent, specifically — not `null`. `null` is the wire form of
        // `Unavailable` ("went dark"), which is a different claim from "did not
        // change", and a host must be able to tell them apart.
        assert!(
            later.get("hp").is_none(),
            "`hp` never changes, so it must not be re-sent: {later}"
        );
        assert!(
            later["frame"].is_i64(),
            "a later event must carry the watch that moved: {later}"
        );
    }
}

/// A game that closes ends the stream: scry notices the target has exited,
/// says so with `detached` / `target_exited`, and exits 5, rather than running
/// on and reporting every watch `null` forever. `--for` is only a safety net
/// here, far longer than the test waits.
#[test]
fn a_target_that_exits_ends_the_stream() {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let (cavia, ready) = spawn_cavia();
    let scratch = Scratch::new();
    let profile = scratch.write(
        "cavia.json",
        &cavia_profile_json(&ready.exe, (ready.player - ready.base) as i64),
    );
    let mut scry = Command::new(env!("CARGO_BIN_EXE_scry"))
        .args(["watch", "--pid"])
        .arg(ready.pid.to_string())
        .arg("--profile")
        .arg(&profile)
        .args([
            "--no-resolve",
            "--format",
            "json",
            "--tick",
            "20",
            "--for",
            "60",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn scry watch");

    std::thread::sleep(Duration::from_millis(300));
    // Kills and reaps the cavia, so it is gone rather than a zombie.
    drop(cavia);

    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = scry.try_wait().expect("poll scry") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "scry kept running after its target exited"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut stdout = String::new();
    scry.stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .expect("read stdout");
    let mut stderr = String::new();
    scry.stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .expect("read stderr");
    assert_eq!(
        status.code(),
        Some(5),
        "exit status for a target that exited\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let last: serde_json::Value =
        serde_json::from_str(stdout.lines().last().expect("some output")).expect("JSON line");
    assert_eq!(last["event"], "detached", "{stdout}");
    assert_eq!(last["reason"], "target_exited", "{stdout}");
}
