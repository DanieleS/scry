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
