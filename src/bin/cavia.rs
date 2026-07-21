//! Test cavia ("guinea pig"): a stand-in for a game process.
//!
//! It reproduces the shape the engine has to cope with in a real game: a
//! **static, module-relative** slot (`PLAYER`, living in the module's data
//! segment) that holds a **pointer** to a heap-allocated struct where the
//! actual values live. Resolving `HP` therefore means: module base + static
//! offset -> dereference -> field offset. Exactly the Tier-1 pointer path.
//!
//! It prints the facts a test needs to derive the path, then parks so the test
//! can read its memory from the outside.

use std::sync::atomic::{AtomicU64, Ordering};

use scry::{LinuxBackend, MemoryBackend};

#[repr(C)]
struct Stats {
    hp: i32,
    hp_max: i32,
}

/// Static slot in the module's data segment. Holds a pointer to `Stats`.
/// Its address is `module_base + <stable offset>`, the anchor a profile stores.
static PLAYER: AtomicU64 = AtomicU64::new(0);

/// A unique byte signature living in the module. Stands in for the recognizable
/// run of bytes a Tier-2 profile scans for to anchor an address. `#[used]` keeps
/// the optimizer from dropping it since nothing reads it.
#[used]
static SIG: [u8; 16] = [
    0x53, 0x43, 0x52, 0x59, 0x5A, 0xA5, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAB,
];

/// The profile's **probe** signature: the run of bytes a resolver scans for to
/// decide this profile fits the process. Distinct from `SIG` (which the Tier-2
/// anchor tests use) so the resolver's identity test is exercised on its own
/// dedicated marker. A profile aimed at a different game/build carries a
/// different probe and simply will not resolve here.
#[used]
static PROBE: [u8; 16] = [
    0x50, 0x52, 0x4F, 0x42, 0x45, 0x5F, 0xA5, 0x5A, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF,
];

/// A build/version marker: a distinct run standing in for the versioned bytes a
/// real profile keys its build discriminant on (a Linux process has no PE
/// version metadata, so the discriminant is expressed in memory instead). The
/// trailing `01 00 02 00 03 00` reads as "v1.2.3".
#[used]
static BUILD: [u8; 12] = [
    0x42, 0x55, 0x49, 0x4C, 0x44, 0x5F, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00,
];

const EXPECTED_HP: i32 = 1337;

fn main() {
    // The "game" allocates its player stats on the heap and records the pointer
    // in the static slot. Leaked so the address stays valid for the process's
    // life (a real game keeps these alive the same way).
    let stats: &'static mut Stats = Box::leak(Box::new(Stats {
        hp: EXPECTED_HP,
        hp_max: 2000,
    }));
    PLAYER.store(stats as *mut Stats as u64, Ordering::SeqCst);

    let pid = std::process::id();
    let exe = std::env::current_exe().expect("current_exe");
    let exe_name = exe
        .file_name()
        .and_then(|f| f.to_str())
        .expect("exe basename")
        .to_string();

    // Dogfood the engine on ourselves to report our own module base.
    let base = LinuxBackend::new(pid as i32)
        .module_base(&exe_name)
        .expect("own module base");

    let player_addr = &PLAYER as *const AtomicU64 as u64;
    let sig_addr = SIG.as_ptr() as u64;
    let probe_addr = PROBE.as_ptr() as u64;
    let build_addr = BUILD.as_ptr() as u64;

    // Machine-readable line the test parses.
    println!(
        "READY pid={pid} exe={exe_name} base=0x{base:x} player=0x{player_addr:x} \
         sig=0x{sig_addr:x} probe=0x{probe_addr:x} build=0x{build_addr:x} hp={EXPECTED_HP}"
    );
    use std::io::Write;
    std::io::stdout().flush().ok();

    // Park; the test reads our memory and then kills us.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
