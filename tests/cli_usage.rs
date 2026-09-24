//! The `scry watch` argument parser refuses what it cannot honour.
//!
//! Each case here was once accepted: an unparseable `--for` meant "run
//! forever", a negative one panicked, and a flag with its value missing was
//! dropped without a word. None of them needs a target process, because the
//! arguments are checked before anything is opened.
//!
//! Only compiled where the CLI has a memory backend; elsewhere `scry watch` is a
//! stub that refuses every invocation alike.
#![cfg(any(target_os = "linux", target_os = "windows"))]

use std::process::{Command, Output};

fn watch(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_scry"))
        .arg("watch")
        .args(args)
        .output()
        .expect("run scry watch")
}

#[test]
fn bad_values_are_usage_errors() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["--pid", "1", "--profile", "p.json", "--for", "ten"],
            "--for needs a number",
        ),
        (
            &["--pid", "1", "--profile", "p.json", "--for", "-1"],
            "--for needs a non-negative",
        ),
        (
            &["--pid", "1", "--profile", "p.json", "--for", "1e30"],
            "--for is too large",
        ),
        (
            &["--pid", "1", "--profile", "p.json", "--for"],
            "--for needs a number",
        ),
        (
            &["--profile", "p.json", "--process"],
            "--process needs an executable name",
        ),
        (
            &["--pid", "1", "--profiles"],
            "--profiles needs a directory",
        ),
    ];
    for (args, message) in cases {
        let out = watch(args);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {stderr}");
        assert!(stderr.contains(message), "{args:?}: {stderr}");
        assert!(
            out.stdout.is_empty(),
            "{args:?}: human mode keeps stdout clean"
        );
    }
}

/// The last stdout line of a JSON-mode run, parsed.
fn last_event(out: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .last()
        .unwrap_or_else(|| panic!("no stdout at all"));
    serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON: {line:?} — {e}"))
}

/// In JSON mode a known failure says what it was on stdout, so a host can tell
/// "no profile fits" from "could not open the game" from a crash. Even a usage
/// error does, wherever `--format json` sits on the line.
#[test]
fn json_mode_ends_a_known_failure_with_an_error_event() {
    let own_pid = std::process::id().to_string();
    let cases: &[(&[&str], &str, i32)] = &[
        (&["--for", "-1", "--format", "json"], "usage", 1),
        (
            &[
                "--format",
                "json",
                "--process",
                "no-such-game-7f3a.exe",
                "--profile",
                "p.json",
            ],
            "no_such_process",
            2,
        ),
        (
            &[
                "--format",
                "json",
                "--pid",
                &own_pid,
                "--profile",
                "/nonexistent/p.json",
            ],
            "profiles_unreadable",
            1,
        ),
    ];
    for (args, code, exit) in cases {
        let out = watch(args);
        assert_eq!(out.status.code(), Some(*exit), "{args:?}");
        let event = last_event(&out);
        assert_eq!(event["event"], "error", "{args:?}: {event}");
        assert_eq!(event["code"], *code, "{args:?}: {event}");
        assert_eq!(event["exit_code"], *exit, "{args:?}: {event}");
        assert!(event["message"].is_string(), "{args:?}: {event}");
        // The person-facing text is still on stderr, unchanged.
        assert!(!out.stderr.is_empty(), "{args:?}");
    }
}
