//! End-to-end proof of the IL2CPP authoring converter.
//!
//! Gated on the `authoring` feature — the tool it exercises is built only there.
//! Two things are proved that the library unit tests can't on their own:
//!
//! 1. the `il2cpp2scry` **binary** turns a `dump.cs` + name map into a profile
//!    the *runtime* parser loads unchanged, and
//! 2. the profile's **derived probe actually resolves** — a `Fake` backend
//!    plants the probe's bytes, and [`resolver::select`] then picks the profile,
//!    exactly as it would against a real game whose metadata carries that name.
//!
//! That second point is the honest stand-in for the live Sea of Stars check: the
//! offsets can only be validated against the running game (a manual, Windows,
//! non-CI step documented in `docs/authoring-il2cpp.md`), but the identity
//! machinery is proved here without one.
#![cfg(feature = "authoring")]

use std::path::PathBuf;
use std::process::Command;

use scry::backend::Region;
use scry::profile::{Rip, Watch};
use scry::{resolver, MemoryBackend, Profile, Result};

const DUMP: &str = "\
// Namespace: Combat
public class PartyMember : MonoBehaviour
{
	public int currentHp; // 0x18
	public int maxHp; // 0x1C
}

// Namespace:
public class GameManager
{
	public int gold; // 0x20
	public PartyMember leader; // 0x28
}
";

const MAP: &str = r#"{
  "label": "Demo (IL2CPP)",
  "process": "Demo.exe",
  "module": "GameAssembly.dll",
  "probe": { "string": "Combat.PartyMember" },
  "watches": [
    { "name": "hp", "tier": "tier1",
      "chain": ["0x2C4E120", "GameManager::leader", "PartyMember::currentHp"],
      "type": "i32", "rate_hz": 10.0 },
    { "name": "gold", "tier": "tier2",
      "anchor": "48 8B 05 ?? ?? ?? ??", "rip": { "disp": 3, "len": 7 },
      "chain": ["GameManager::gold"], "type": "u32" }
  ]
}"#;

/// A unique scratch directory for this test process, cleaned up on the way out.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("il2cpp2scry-{}", std::process::id()));
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

/// Run the binary against the fixtures and parse the profile it prints.
fn run_converter() -> Profile {
    let scratch = Scratch::new();
    let dump = scratch.write("dump.cs", DUMP);
    let map = scratch.write("map.json", MAP);

    let out = Command::new(env!("CARGO_BIN_EXE_il2cpp2scry"))
        .arg("--dump")
        .arg(&dump)
        .arg("--map")
        .arg(&map)
        .output()
        .expect("run il2cpp2scry");
    assert!(
        out.status.success(),
        "converter failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Profile::from_json(&String::from_utf8(out.stdout).expect("utf8 stdout"))
        .expect("the runtime must parse the emitted profile")
}

#[test]
fn binary_emits_a_runtime_loadable_profile_with_resolved_offsets() {
    let profile = run_converter();

    assert_eq!(profile.label.as_deref(), Some("Demo (IL2CPP)"));
    assert_eq!(profile.match_.process, "Demo.exe");

    match &profile.watches[0] {
        Watch::Tier1 {
            name,
            module,
            offsets,
            ..
        } => {
            assert_eq!(name, "hp");
            assert_eq!(module, "GameAssembly.dll");
            // 0x2C4E120 passthrough, then leader (0x28) then currentHp (0x18).
            assert_eq!(offsets, &vec![0x2C4E120, 0x28, 0x18]);
        }
        other => panic!("expected Tier1, got {other:?}"),
    }
    match &profile.watches[1] {
        Watch::Tier2 {
            name, rip, offsets, ..
        } => {
            assert_eq!(name, "gold");
            assert_eq!(*rip, Some(Rip { disp: 3, len: 7 })); // carried through
            assert_eq!(offsets, &vec![0x20]); // GameManager::gold
        }
        other => panic!("expected Tier2, got {other:?}"),
    }
}

/// A deterministic in-memory backend: one readable region holding `mem` mapped
/// at `base`. Enough for the resolver to run its probe scan against.
struct Fake {
    base: u64,
    mem: Vec<u8>,
}

impl MemoryBackend for Fake {
    fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> Result<()> {
        let start = (addr - self.base) as usize;
        buf.copy_from_slice(&self.mem[start..start + buf.len()]);
        Ok(())
    }
    fn module_base(&self, _name: &str) -> Result<u64> {
        Ok(self.base)
    }
    fn readable_regions(&self) -> Result<Vec<Region>> {
        Ok(vec![Region {
            start: self.base,
            len: self.mem.len() as u64,
        }])
    }
}

#[test]
fn the_derived_probe_actually_resolves_against_matching_memory() {
    let profile = run_converter();

    // Plant the exact bytes the string-derived probe scans for — "Combat.
    // PartyMember" as UTF-8 — inside some filler, the way a game's loaded
    // metadata would carry it.
    let needle = b"Combat.PartyMember";
    let mut mem = vec![0u8; 512];
    mem[200..200 + needle.len()].copy_from_slice(needle);
    let backend = Fake {
        base: 0x1_0000,
        mem,
    };

    let picked =
        resolver::select(&backend, "Demo.exe", std::slice::from_ref(&profile)).expect("select ok");
    assert_eq!(
        picked.and_then(|p| p.label.as_deref()),
        Some("Demo (IL2CPP)"),
        "a converter-derived probe must resolve in memory that contains its marker"
    );

    // And the fail-safe still holds: memory *without* the marker selects nothing.
    let empty = Fake {
        base: 0x1_0000,
        mem: vec![0u8; 512],
    };
    let none = resolver::select(&empty, "Demo.exe", std::slice::from_ref(&profile)).expect("ok");
    assert!(none.is_none(), "no marker in memory must mean no match");
}
