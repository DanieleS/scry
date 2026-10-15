//! AOB (array-of-bytes) signature scanning.
//!
//! Many game values can't be reached from a static, module-relative offset —
//! the anchoring address is found instead by matching a **byte signature** (a
//! run of machine-code or data bytes, with wildcards for the parts that vary
//! between builds or runs). This is the Tier-2 mechanic: scan the target for a
//! signature, get an address, then resolve offsets from there exactly as Tier-1
//! does via [`MemoryBackend::resolve`].
//!
//! Scanning is done once at attach and the result cached — never per poll.

use crate::backend::MemoryBackend;
use crate::error::{Error, Result};

/// One byte of a signature: a concrete value, or a wildcard that matches any.
pub type PatternByte = Option<u8>;

/// Parse a signature string like `"48 8B 05 ?? ?? ?? ?? 48 8B 88"` into a
/// pattern. Tokens are whitespace-separated; `??` (or `?`) is a wildcard.
pub fn parse_pattern(sig: &str) -> Result<Vec<PatternByte>> {
    let pattern: Result<Vec<PatternByte>> = sig
        .split_whitespace()
        .map(|tok| match tok {
            "??" | "?" => Ok(None),
            hex => u8::from_str_radix(hex, 16)
                .map(Some)
                .map_err(|_| Error::BadSignature(format!("not a hex byte: {tok:?}"))),
        })
        .collect();
    let pattern = pattern?;
    if pattern.is_empty() {
        return Err(Error::BadSignature("empty signature".to_string()));
    }
    Ok(pattern)
}

/// Find the first offset in `haystack` where `pattern` matches. Wildcard bytes
/// match anything.
pub fn find_in_buffer(haystack: &[u8], pattern: &[PatternByte]) -> Option<usize> {
    if pattern.is_empty() || pattern.len() > haystack.len() {
        return None;
    }
    let last = haystack.len() - pattern.len();
    'candidate: for i in 0..=last {
        for (j, want) in pattern.iter().enumerate() {
            if let Some(byte) = want {
                if haystack[i + j] != *byte {
                    continue 'candidate;
                }
            }
        }
        return Some(i);
    }
    None
}

/// Scan the whole target process for `pattern`, returning the absolute address
/// of the first match. Reads region by region in bounded chunks, overlapping by
/// `pattern.len() - 1` so a match straddling a chunk boundary is not missed.
/// Regions that fail to read (special mappings, torn-down pages) are skipped
/// rather than aborting the scan.
pub fn find_in_process<B: MemoryBackend + ?Sized>(
    backend: &B,
    pattern: &[PatternByte],
) -> Result<Option<u64>> {
    find_in_process_from(backend, pattern, None)
}

/// [`find_in_process`], but scanning the regions that end above `first` before
/// the rest — still in address order within each group, and still the whole
/// process in the end, so the answer is found wherever it is.
///
/// What it changes is how long that takes. A signature normally lives in one
/// module's image, and a 64-bit loader maps executables near the top of the
/// address space, above every heap: a game with gigabytes allocated would have
/// all of them read before the scan reached its code. Passing that module's
/// base puts its image first. The first match in that order is returned, which
/// is the first match *in the module* when there is one; a signature meant to
/// be unique has one either way.
pub fn find_in_process_from<B: MemoryBackend + ?Sized>(
    backend: &B,
    pattern: &[PatternByte],
    first: Option<u64>,
) -> Result<Option<u64>> {
    if pattern.is_empty() {
        return Err(Error::BadSignature("empty signature".to_string()));
    }
    const CHUNK: usize = 1 << 20; // 1 MiB
    let overlap = (pattern.len() - 1) as u64;

    let mut regions = backend.readable_regions()?;
    if let Some(start) = first {
        // Stable, so each group keeps its address order.
        regions.sort_by_key(|r| r.start.saturating_add(r.len) <= start);
    }

    for region in regions {
        let end = region.start.saturating_add(region.len);
        let mut addr = region.start;
        while addr < end {
            let want = std::cmp::min(CHUNK as u64, end - addr) as usize;
            if want < pattern.len() {
                break;
            }
            let mut buf = vec![0u8; want];
            if backend.read_bytes(addr, &mut buf).is_err() {
                // Region not actually readable through the OS primitive; skip it.
                break;
            }
            if let Some(hit) = find_in_buffer(&buf, pattern) {
                return Ok(Some(addr + hit as u64));
            }
            if (want as u64) < CHUNK as u64 {
                break; // was the final, short chunk
            }
            addr += CHUNK as u64 - overlap;
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bytes_and_wildcards() {
        let p = parse_pattern("48 8B ?? 90 ?").unwrap();
        assert_eq!(p, vec![Some(0x48), Some(0x8B), None, Some(0x90), None]);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_pattern("48 ZZ").is_err());
        assert!(parse_pattern("   ").is_err());
    }

    #[test]
    fn matches_with_wildcards() {
        let hay = [0x00, 0x48, 0x8B, 0x77, 0x90, 0xFF];
        let pat = parse_pattern("48 8B ?? 90").unwrap();
        assert_eq!(find_in_buffer(&hay, &pat), Some(1));
    }

    /// Two regions, a "heap" low and a "module" high, each holding `sig` once,
    /// and a record of which region each read touched.
    struct TwoRegions {
        low: Vec<u8>,
        high: Vec<u8>,
        reads: std::cell::RefCell<Vec<u64>>,
    }

    const LOW: u64 = 0x1000_0000;
    const HIGH: u64 = 0x7FF7_0000_0000;

    impl TwoRegions {
        fn new(sig: &[u8]) -> Self {
            let mut low = vec![0u8; 4096];
            let mut high = vec![0u8; 4096];
            low[100..100 + sig.len()].copy_from_slice(sig);
            high[200..200 + sig.len()].copy_from_slice(sig);
            TwoRegions {
                low,
                high,
                reads: Default::default(),
            }
        }
    }

    impl MemoryBackend for TwoRegions {
        fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> Result<()> {
            self.reads.borrow_mut().push(addr);
            let (base, mem) = if addr >= HIGH {
                (HIGH, &self.high)
            } else {
                (LOW, &self.low)
            };
            let at = (addr - base) as usize;
            buf.copy_from_slice(&mem[at..at + buf.len()]);
            Ok(())
        }
        fn module_base(&self, _name: &str) -> Result<u64> {
            Ok(HIGH)
        }
        fn readable_regions(&self) -> Result<Vec<crate::backend::Region>> {
            Ok(vec![
                crate::backend::Region {
                    start: LOW,
                    len: 4096,
                },
                crate::backend::Region {
                    start: HIGH,
                    len: 4096,
                },
            ])
        }
    }

    #[test]
    fn a_scan_from_a_module_reads_that_module_first() {
        let fake = TwoRegions::new(&[0xDE, 0xAD, 0xBE, 0xEF]);
        let pat = parse_pattern("DE AD BE EF").unwrap();

        let hit = find_in_process_from(&fake, &pat, Some(HIGH)).unwrap();
        assert_eq!(hit, Some(HIGH + 200), "the module's match, found first");
        assert!(
            fake.reads.borrow().iter().all(|&a| a >= HIGH),
            "the heap below was never read"
        );

        // Without a starting point the scan is what it always was: address order.
        assert_eq!(find_in_process(&fake, &pat).unwrap(), Some(LOW + 100));
    }

    /// A module that does not hold the signature still leaves the rest of the
    /// process to be scanned: the order changes, the answer does not.
    #[test]
    fn a_scan_from_a_module_still_finds_a_match_outside_it() {
        let mut fake = TwoRegions::new(&[0xDE, 0xAD, 0xBE, 0xEF]);
        fake.high.fill(0);
        let pat = parse_pattern("DE AD BE EF").unwrap();
        let hit = find_in_process_from(&fake, &pat, Some(HIGH)).unwrap();
        assert_eq!(hit, Some(LOW + 100));
    }

    #[test]
    fn no_false_match() {
        let hay = [0x48, 0x8B, 0x05];
        let pat = parse_pattern("48 8B 06").unwrap();
        assert_eq!(find_in_buffer(&hay, &pat), None);
    }
}
