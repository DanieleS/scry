//! Test cavia ("guinea pig"): a stand-in for a game process.
//!
//! It reproduces the shape the engine has to cope with in a real game: a
//! **static, module-relative** slot (`PLAYER`, living in the module's data
//! segment) that holds a **pointer** to a heap-allocated struct where the
//! actual values live. Resolving `HP` therefore means: module base + static
//! offset -> dereference -> field offset. Exactly the Tier-1 pointer path.
//!
//! It also plants a real x64 **RIP-relative accessor** — a `mov rax,
//! [rip+disp32]` whose operand is that same `PLAYER` slot — so the Tier-2 path
//! that decodes a displacement into a static base (the shape on a modern 64-bit
//! build) can be proven against a genuine instruction, not a mock.
//!
//! It prints the facts a test needs to derive the path, then parks so the test
//! can read its memory from the outside.

use std::sync::atomic::{AtomicI32, AtomicU64, AtomicU8, Ordering};

#[cfg(any(target_os = "linux", target_os = "windows"))]
use scry::MemoryBackend;

#[repr(C)]
struct Stats {
    hp: i32,
    hp_max: i32,
    /// A value that changes over the process's life — the cavia bumps it on a
    /// timer, standing in for a live game stat. It gives the polling loop a real
    /// diff to observe. `repr(C)` fixes it at offset 8; `AtomicI32` shares i32's
    /// layout, so an outside reader sees a plain 4-byte int there.
    frame: AtomicI32,
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

/// A real x64 RIP-relative accessor, planted in the module's own data — the
/// shape a static base takes on a 64-bit build: `mov rax, [rip+disp32]`
/// (`48 8B 05 <disp32>`) whose operand is the `PLAYER` slot, followed by a unique
/// tail (`C3 90 5A A5 5A A5`) so a scan pins it unambiguously (the opcode alone
/// occurs all over real code). It lives in the module, **not** the heap, so it is
/// guaranteed within the ±2 GiB a signed `disp32` can encode of `PLAYER` — the
/// invariant a real compiler always satisfies, and which a heap-leaked buffer can
/// violate on a 64-bit target where the heap sits far from the image. The
/// displacement is filled at runtime from the two live addresses; `AtomicU8`
/// shares `u8`'s layout, so an outside reader sees plain bytes.
static RIPSTUB: [AtomicU8; 13] = [
    AtomicU8::new(0x48),
    AtomicU8::new(0x8B),
    AtomicU8::new(0x05),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0xC3),
    AtomicU8::new(0x90),
    AtomicU8::new(0x5A),
    AtomicU8::new(0xA5),
    AtomicU8::new(0x5A),
    AtomicU8::new(0xA5),
];

const EXPECTED_HP: i32 = 1337;

fn main() {
    // The "game" allocates its player stats on the heap and records the pointer
    // in the static slot. Leaked so the address stays valid for the process's
    // life (a real game keeps these alive the same way).
    let stats: &'static Stats = Box::leak(Box::new(Stats {
        hp: EXPECTED_HP,
        hp_max: 2000,
        frame: AtomicI32::new(0),
    }));
    PLAYER.store(stats as *const Stats as u64, Ordering::SeqCst);

    let pid = std::process::id();
    let exe = std::env::current_exe().expect("current_exe");
    let exe_name = exe
        .file_name()
        .and_then(|f| f.to_str())
        .expect("exe basename")
        .to_string();

    // Dogfood the engine on ourselves to report our own module base, through
    // whichever backend this OS was built with — the same read path a host takes
    // against a game. On a platform with no backend there is nothing to dogfood
    // (and no integration test runs there), so the base is reported as 0.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let base = scry::open_host(pid)
        .expect("open self")
        .module_base(&exe_name)
        .expect("own module base");
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let base: u64 = 0;

    let player_addr = &PLAYER as *const AtomicU64 as u64;
    let sig_addr = SIG.as_ptr() as u64;
    let probe_addr = PROBE.as_ptr() as u64;
    let build_addr = BUILD.as_ptr() as u64;

    // Fill in the RIP displacement now that both addresses are live: the operand
    // must resolve to PLAYER via `anchor + 7 + disp32`, exactly the value a
    // compiler would have baked in. RIPSTUB and PLAYER both live in this module,
    // so the delta always fits the signed 32-bit field.
    let rip_addr = {
        let addr = &RIPSTUB as *const _ as u64;
        let disp32 = (player_addr as i64 - (addr as i64 + 7)) as i32;
        let disp = disp32.to_le_bytes();
        for (slot, byte) in RIPSTUB[3..7].iter().zip(disp) {
            slot.store(byte, Ordering::SeqCst);
        }
        addr
    };

    // Machine-readable line the test parses.
    println!(
        "READY pid={pid} exe={exe_name} base=0x{base:x} player=0x{player_addr:x} \
         sig=0x{sig_addr:x} probe=0x{probe_addr:x} build=0x{build_addr:x} \
         rip=0x{rip_addr:x} hp={EXPECTED_HP}"
    );
    use std::io::Write;
    std::io::stdout().flush().ok();

    // Park, but keep a value moving: a poller attached from the outside must be
    // able to watch `frame` change between reads. The test reads our memory and
    // then kills us.
    loop {
        std::thread::sleep(std::time::Duration::from_millis(20));
        stats.frame.fetch_add(1, Ordering::SeqCst);
    }
}
