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

    // Machine-readable line the test parses.
    println!(
        "READY pid={pid} exe={exe_name} base=0x{base:x} player=0x{player_addr:x} hp={EXPECTED_HP}"
    );
    use std::io::Write;
    std::io::stdout().flush().ok();

    // Park; the test reads our memory and then kills us.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
