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
    if pattern.is_empty() {
        return Err(Error::BadSignature("empty signature".to_string()));
    }
    const CHUNK: usize = 1 << 20; // 1 MiB
    let overlap = (pattern.len() - 1) as u64;

    for region in backend.readable_regions()? {
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

    #[test]
    fn no_false_match() {
        let hay = [0x48, 0x8B, 0x05];
        let pat = parse_pattern("48 8B 06").unwrap();
        assert_eq!(find_in_buffer(&hay, &pat), None);
    }
}
