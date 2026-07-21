//! End-to-end proof of the polling loop against a real, separate process.
//!
//! The cavia holds a static `HP` (never changes) and a `frame` counter it bumps
//! on a timer. Attaching to it exactly as a host would, these tests assert the
//! loop's contract: the first poll reports the values, a value that *changes*
//! shows up in the diff, and one that doesn't is silent — and the same holds
//! when the loop drives itself on its own thread and delivers over a channel.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use scry::profile::{Match, Profile, ValueType, Watch};
use scry::{Config, LinuxBackend, Session, Value};

mod common;
use common::spawn_cavia;

// The cavia's probe marker; real, though the loop attaches directly and never
// runs the probe itself.
const PROBE_SIG: &str = "50 52 4F 42 45 5F A5 5A 01 23 45 67 89 AB CD EF";

/// `frame` sits at offset 8 within `Stats` (`hp`, `hp_max`, `frame`), reached
/// as `[player_offset, 8]`; `hp` at offset 0 as `[player_offset, 0]`.
const FRAME_FIELD: i64 = 8;
const HP_FIELD: i64 = 0;

/// Build a two-watch profile over the cavia: the moving `frame` and the fixed
/// `hp`, both Tier-1 paths from the module base through `PLAYER`.
fn cavia_profile(exe: &str, player_offset: i64) -> Profile {
    Profile {
        label: Some("cavia".to_string()),
        match_: Match {
            process: exe.to_string(),
            module: exe.to_string(),
            version: None,
            probe: PROBE_SIG.to_string(),
        },
        watches: vec![
            Watch::Tier1 {
                name: "frame".to_string(),
                module: exe.to_string(),
                offsets: vec![player_offset, FRAME_FIELD],
                ty: ValueType::I32,
                rate_hz: None,
            },
            Watch::Tier1 {
                name: "hp".to_string(),
                module: exe.to_string(),
                offsets: vec![player_offset, HP_FIELD],
                ty: ValueType::I32,
                rate_hz: None,
            },
        ],
    }
}

fn as_i32(v: Option<&Value>) -> Option<i32> {
    match v {
        Some(Value::I32(n)) => Some(*n),
        _ => None,
    }
}

#[test]
fn diff_reports_a_changed_value_and_stays_silent_on_an_unchanged_one() {
    let (_cavia, ready) = spawn_cavia();
    let be = LinuxBackend::new(ready.pid);
    let player_offset = (ready.player - ready.base) as i64;
    let profile = cavia_profile(&ready.exe, player_offset);

    let mut session = Session::attach(be, &profile, Config::default());

    // First poll: both values seen for the first time, so both are emitted.
    let first = session.poll(Duration::ZERO);
    assert_eq!(as_i32(first.get("hp")), Some(ready.hp));
    let frame0 = as_i32(first.get("frame")).expect("frame reported on first poll");

    // Let the cavia bump `frame` a few times (it steps every ~20 ms).
    std::thread::sleep(Duration::from_millis(120));

    let second = session.poll(Duration::from_millis(200));
    // hp never changed -> it must not appear in the diff.
    assert!(
        !second.contains_key("hp"),
        "an unchanged value must not be re-emitted, got {second:?}"
    );
    // frame did change -> it must appear, and it must have advanced.
    let frame1 = as_i32(second.get("frame")).expect("changed frame must be in the diff");
    assert!(
        frame1 > frame0,
        "frame should have advanced: {frame0} -> {frame1}"
    );
}

#[test]
fn run_delivers_diffs_over_a_channel_until_stopped() {
    let (_cavia, ready) = spawn_cavia();
    let be = LinuxBackend::new(ready.pid);
    let player_offset = (ready.player - ready.base) as i64;
    let profile = cavia_profile(&ready.exe, player_offset);

    // A channel is just a forwarding callback — the acceptance's "callback/
    // channel" delivery, exercised on the real threaded loop.
    let (tx, rx) = mpsc::channel();
    let config = Config {
        base_tick: Duration::from_millis(20),
        ..Config::default()
    };
    let handle = Session::attach(be, &profile, config).run(move |diff| {
        let _ = tx.send(diff);
    });

    // Collect a few frame updates within a generous window.
    let mut frames = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while frames.len() < 3 && Instant::now() < deadline {
        if let Ok(diff) = rx.recv_timeout(Duration::from_millis(500)) {
            if let Some(f) = as_i32(diff.get("frame")) {
                frames.push(f);
            }
        }
    }

    assert!(
        frames.len() >= 2,
        "expected multiple frame updates over the channel, got {frames:?}"
    );
    assert!(
        frames.windows(2).all(|w| w[1] > w[0]),
        "frame updates should arrive strictly increasing, got {frames:?}"
    );

    // Dropping the handle stops and joins the loop thread.
    drop(handle);
}
