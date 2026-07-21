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

use crate::error::{Error, Result};

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
