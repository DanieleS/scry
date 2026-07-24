//! The one and only capability surface the engine has over a target process.
//!
//! A backend supplies two raw operations — read bytes at an address, and locate
//! a module's load base — plus its pointer width. Everything else (typed reads,
//! pointer-chain resolution) is built on top as provided methods, so a new
//! platform only has to implement the two primitives.
//!
//! Note what is *absent*: there is no `write`, no `alloc`, no `execute`. The
//! trait cannot express a mutation of the target. That is the whole point.

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "windows")]
pub mod windows;

pub mod pe;

use crate::error::{Error, Result};

/// Byte offset, within an IL2CPP `System.String` object, of its 32-bit length.
const STRING_LEN_OFFSET: u64 = 0x10;
/// Byte offset, within an IL2CPP `System.String` object, of its UTF-16 payload.
const STRING_CHARS_OFFSET: u64 = 0x14;
/// Hard cap on UTF-16 code units read for a string — a garbage length can't
/// drive an unbounded read. 512 units is 1 KiB, ample for any name/label.
const STRING_MAX_UTF16: usize = 512;

/// A contiguous span of the target's address space.
#[derive(Debug, Clone, Copy)]
pub struct Region {
    pub start: u64,
    pub len: u64,
}

pub trait MemoryBackend {
    /// Fill `buf` with bytes read from `addr` in the target. Must fail (rather
    /// than partially fill) if the full range cannot be read.
    fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> Result<()>;

    /// Load base of the mapped module `name` (e.g. `"game.exe"`), by which
    /// static, module-relative addresses are anchored.
    fn module_base(&self, name: &str) -> Result<u64>;

    /// The target's readable memory regions, in ascending address order — the
    /// haystack an AOB scan searches.
    fn readable_regions(&self) -> Result<Vec<Region>>;

    /// Best-effort build/version identifier for module `name` (e.g. a PE
    /// version string), used by the resolver to cheaply narrow same-executable
    /// candidates before the authoritative probe test.
    ///
    /// Returns `None` when the platform can't supply one — the default, and the
    /// honest answer on Linux, where there is no PE metadata. A `None` here
    /// never causes a wrong match: the resolver simply skips version filtering
    /// and lets the probe decide.
    fn module_version(&self, _name: &str) -> Result<Option<String>> {
        Ok(None)
    }

    /// Pointer width of the target, in bytes. 64-bit unless a backend says
    /// otherwise.
    fn pointer_size(&self) -> usize {
        8
    }

    fn read_u32(&self, addr: u64) -> Result<u32> {
        let mut b = [0u8; 4];
        self.read_bytes(addr, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn read_i32(&self, addr: u64) -> Result<i32> {
        Ok(self.read_u32(addr)? as i32)
    }

    fn read_f32(&self, addr: u64) -> Result<f32> {
        Ok(f32::from_bits(self.read_u32(addr)?))
    }

    fn read_u64(&self, addr: u64) -> Result<u64> {
        let mut b = [0u8; 8];
        self.read_bytes(addr, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    /// Read a pointer-sized value and widen it to `u64`.
    fn read_ptr(&self, addr: u64) -> Result<u64> {
        match self.pointer_size() {
            8 => self.read_u64(addr),
            4 => Ok(self.read_u32(addr)? as u64),
            other => Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("unsupported pointer size: {other}"),
            ))),
        }
    }

    /// Read an IL2CPP `System.String` *referenced* at `addr`.
    ///
    /// C# strings are reference types, so a pointer-chain that reaches a string
    /// field lands on the slot **holding a pointer** to the string object, not on
    /// the object itself. This reads that pointer, then decodes the object: a
    /// 32-bit length at `+0x10` and the UTF-16 payload at `+0x14`. The length is
    /// clamped to [`STRING_MAX_UTF16`] so a bogus value can't run away, and the
    /// bytes are decoded lossily — an invalid unit becomes U+FFFD rather than an
    /// error, keeping a live read from going dark on one corrupt character.
    ///
    /// A null reference is an empty string, not a failure — a party slot with no
    /// character reads as `""`, which is honest state, not "unavailable".
    fn read_string(&self, addr: u64) -> Result<String> {
        let object = self.read_ptr(addr)?;
        if object == 0 {
            return Ok(String::new());
        }
        let len = self.read_i32(object.wrapping_add(STRING_LEN_OFFSET))?;
        let units = (len.max(0) as usize).min(STRING_MAX_UTF16);
        if units == 0 {
            return Ok(String::new());
        }
        let mut bytes = vec![0u8; units * 2];
        self.read_bytes(object.wrapping_add(STRING_CHARS_OFFSET), &mut bytes)?;
        let wide: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Ok(String::from_utf16_lossy(&wide))
    }

    /// Decode a RIP-relative reference at `anchor` and return the operand's
    /// effective address.
    ///
    /// Reads the signed 32-bit displacement at `anchor + disp` and returns
    /// `anchor + len + displacement` — the address an x64 `[rip+disp32]` operand
    /// actually points at, given that the anchor is the instruction's start and
    /// RIP addressing is relative to the *next* instruction (`anchor + len`).
    /// This is how a Tier-2 AOB hit on a `mov reg, [rip+disp32]` becomes the
    /// static base its pointer chain walks from.
    ///
    /// The displacement read can fail (an anchor no longer mapped); that
    /// propagates as an error, so a stale signature yields "unavailable" rather
    /// than a bogus address treated as real.
    fn resolve_rip(&self, anchor: u64, disp: i64, len: i64) -> Result<u64> {
        let displacement = self.read_i32(anchor.wrapping_add(disp as u64))?;
        Ok(anchor
            .wrapping_add(len as u64)
            .wrapping_add(displacement as i64 as u64))
    }

    /// Walk a pointer chain and return the **address** the value lives at.
    ///
    /// Semantics (matching how Cheat Engine tables describe a path): starting
    /// from `start`, each offset is added, and the result is dereferenced —
    /// except after the *last* offset, where we stop. So a value reached as
    /// `[[start + o0] + o1]` is described by `offsets = [o0, o1]`, and the
    /// caller reads the typed value at the returned address.
    ///
    /// A broken hop (any dereference that fails) propagates as an error, so a
    /// chain that no longer resolves yields "unavailable", never a garbage
    /// address treated as real.
    fn resolve(&self, start: u64, offsets: &[i64]) -> Result<u64> {
        let mut addr = start;
        let last = offsets.len().saturating_sub(1);
        for (i, &off) in offsets.iter().enumerate() {
            addr = addr.wrapping_add(off as u64);
            if i != last {
                addr = self.read_ptr(addr)?;
            }
        }
        Ok(addr)
    }
}
