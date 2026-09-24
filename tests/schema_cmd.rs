//! `scry schema` from the outside: the command a profile repository runs in CI
//! to regenerate a contract's schema.
//!
//! Unlike the other binary tests this one needs no target process, so it runs
//! on every platform — which is the point of the command.

use std::path::PathBuf;
use std::process::Command;

fn scratch(name: &str, contents: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("scry-schema-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let path = dir.join(name);
    std::fs::write(&path, contents).expect("write scratch file");
    path
}

fn schema(path: &PathBuf) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_scry"))
        .arg("schema")
        .arg(path)
        .output()
        .expect("run scry schema")
}

const PROFILE: &str = r#"{
  "label": "Example (Steam)",
  "contract": { "id": "example", "version": "1.2" },
  "match": { "process": "g.exe", "module": "g.exe", "probe": "90 90" },
  "watches": [
    { "tier": "tier1", "name": "hp", "module": "g.exe", "offsets": [16], "type": "i32" },
    { "tier": "record", "name": "player",
      "base": { "tier": "tier1", "module": "g.exe", "offsets": [40] },
      "fields": { "sp": { "type": "u32" } } }
  ]
}"#;

#[test]
fn prints_a_deterministic_schema_named_by_the_contract() {
    let path = scratch("example.json", PROFILE);
    let first = schema(&path);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let out = String::from_utf8(first.stdout).expect("utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(&out).expect("stdout is one JSON document");
    assert_eq!(parsed["$id"], "urn:scry:contract:example:1.2");
    assert_eq!(parsed["properties"]["hp"]["type"][0], "integer");
    assert_eq!(parsed["properties"]["player"]["required"][0], "sp");

    // Byte-for-byte the same on a second run: it gets committed and diffed.
    let second = schema(&path);
    assert_eq!(out.as_bytes(), second.stdout.as_slice());
}

#[test]
fn rejects_an_invalid_profile_on_stderr() {
    let path = scratch(
        "bad.json",
        r#"{ "contract": { "id": "Bad Id", "version": "1.0" },
             "match": { "process": "g.exe", "module": "g.exe", "probe": "90" },
             "watches": [] }"#,
    );
    let out = schema(&path);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "nothing on stdout when it fails");
    assert!(String::from_utf8_lossy(&out.stderr).contains("slug"));
}
