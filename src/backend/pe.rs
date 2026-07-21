//! Minimal PE-header reader: derive a stable per-build identifier for a loaded
//! module from its headers in memory, using only the read primitive.
//!
//! Windows exposes no single "version" a profile author would naturally reach
//! for, so the [resolver](crate::resolver)'s optional version discriminant keys
//! on the two header fields that are cheap to read and change on every rebuild:
//! the COFF `TimeDateStamp` and the optional header's `SizeOfImage`. Together
//! they identify a build at exactly the granularity the discriminant wants.
//!
//! This lives platform-independent — and is unit-tested on Linux against a
//! synthetic header — precisely because it is pure logic over
//! [`MemoryBackend::read_bytes`]. Only the raw Win32 calls that hand it a real
//! module base are Windows-only.

use std::io;

use crate::backend::MemoryBackend;
use crate::error::{Error, Result};

// Structural offsets from the PE/COFF specification.
const DOS_MAGIC: u16 = 0x5A4D; // "MZ", little-endian
const DOS_E_LFANEW: u64 = 0x3C; // u32: distance from image base to the PE header
const PE_SIGNATURE: u32 = 0x0000_4550; // "PE\0\0", little-endian
const FILE_HEADER_TIMEDATESTAMP: u64 = 4 + 8; // PE sig (4) + offset of TimeDateStamp in IMAGE_FILE_HEADER (8)
const OPTIONAL_SIZEOFIMAGE: u64 = 4 + 20 + 56; // PE sig (4) + IMAGE_FILE_HEADER (20) + SizeOfImage offset (56)

/// Read a module's build identifier — `"pe:<timestamp>:<sizeofimage>"` — from
/// its headers at `module_base`.
///
/// Anything that isn't a well-formed PE image (no `MZ`, no `PE\0\0`) surfaces as
/// an error rather than a fabricated identifier, so a wrong base can never masquerade
/// as a matching build.
pub fn build_id<B: MemoryBackend + ?Sized>(be: &B, module_base: u64) -> Result<String> {
    let mut magic = [0u8; 2];
    be.read_bytes(module_base, &mut magic)?;
    if u16::from_le_bytes(magic) != DOS_MAGIC {
        return Err(not_a_pe(module_base, "no MZ header"));
    }

    let e_lfanew = be.read_u32(module_base + DOS_E_LFANEW)? as u64;
    let pe = module_base + e_lfanew;
    if be.read_u32(pe)? != PE_SIGNATURE {
        return Err(not_a_pe(module_base, "no PE signature"));
    }

    let timestamp = be.read_u32(pe + FILE_HEADER_TIMEDATESTAMP)?;
    let size_of_image = be.read_u32(pe + OPTIONAL_SIZEOFIMAGE)?;
    Ok(format!("pe:{timestamp:08x}:{size_of_image:08x}"))
}

fn not_a_pe(base: u64, why: &str) -> Error {
    Error::Io(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("module at 0x{base:x} is not a PE image: {why}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{MemoryBackend, Region};

    /// An in-memory backend serving a fixed buffer mapped at `base` — enough to
    /// feed `build_id` a synthetic PE header without any OS involvement.
    struct Fake {
        base: u64,
        mem: Vec<u8>,
    }

    impl MemoryBackend for Fake {
        fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> Result<()> {
            let start = (addr - self.base) as usize;
            let end = start + buf.len();
            if end > self.mem.len() {
                return Err(Error::ShortRead {
                    expected: buf.len(),
                    got: 0,
                });
            }
            buf.copy_from_slice(&self.mem[start..end]);
            Ok(())
        }
        fn module_base(&self, _name: &str) -> Result<u64> {
            Ok(self.base)
        }
        fn readable_regions(&self) -> Result<Vec<Region>> {
            Ok(vec![])
        }
    }

    /// Lay down a minimal-but-valid PE header: `MZ`, an `e_lfanew` pointing at a
    /// `PE\0\0` signature, then a `TimeDateStamp` and `SizeOfImage` at their spec
    /// offsets.
    fn synthetic_pe(timestamp: u32, size_of_image: u32) -> Vec<u8> {
        let e_lfanew: u32 = 0x80;
        let mut mem = vec![0u8; 0x200];
        mem[0..2].copy_from_slice(&DOS_MAGIC.to_le_bytes());
        mem[0x3C..0x40].copy_from_slice(&e_lfanew.to_le_bytes());
        let pe = e_lfanew as usize;
        mem[pe..pe + 4].copy_from_slice(&PE_SIGNATURE.to_le_bytes());
        let ts = pe + FILE_HEADER_TIMEDATESTAMP as usize;
        mem[ts..ts + 4].copy_from_slice(&timestamp.to_le_bytes());
        let soi = pe + OPTIONAL_SIZEOFIMAGE as usize;
        mem[soi..soi + 4].copy_from_slice(&size_of_image.to_le_bytes());
        mem
    }

    #[test]
    fn reads_build_id_from_pe_headers() {
        let be = Fake {
            base: 0x1_4000_0000,
            mem: synthetic_pe(0x1234_ABCD, 0x0010_0000),
        };
        assert_eq!(build_id(&be, be.base).unwrap(), "pe:1234abcd:00100000");
    }

    #[test]
    fn different_builds_get_different_ids() {
        let a = Fake {
            base: 0x400000,
            mem: synthetic_pe(0x1111_1111, 0x2000),
        };
        let b = Fake {
            base: 0x400000,
            mem: synthetic_pe(0x2222_2222, 0x2000),
        };
        assert_ne!(build_id(&a, a.base).unwrap(), build_id(&b, b.base).unwrap());
    }

    #[test]
    fn rejects_non_pe_memory() {
        // Right size, but no MZ magic — must not invent an identifier.
        let be = Fake {
            base: 0x400000,
            mem: vec![0u8; 0x200],
        };
        assert!(build_id(&be, be.base).is_err());
    }

    #[test]
    fn rejects_mz_without_pe_signature() {
        let mut mem = vec![0u8; 0x200];
        mem[0..2].copy_from_slice(&DOS_MAGIC.to_le_bytes());
        mem[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        // ...but the bytes at e_lfanew are left zero, not "PE\0\0".
        let be = Fake {
            base: 0x400000,
            mem,
        };
        assert!(build_id(&be, be.base).is_err());
    }
}
