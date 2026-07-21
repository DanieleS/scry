//! Linux backend, used for **development and testing** of the engine core.
//!
//! Production targets are Windows games (a `WindowsBackend` will implement the
//! same trait over `ReadProcessMemory`), but the engine's actual logic —
//! pointer-chain resolution, signature scanning, the polling loop — is
//! platform-independent. Reading a Linux cavia process here through
//! `process_vm_readv` exercises all of it, in this container, with real memory.

use std::fs;
use std::io;
use std::os::raw::{c_int, c_ulong, c_void};

use crate::backend::{MemoryBackend, Region};
use crate::error::{Error, Result};

// Declared directly against the system libc so the crate needs no dependencies.
#[repr(C)]
struct IoVec {
    base: *mut c_void,
    len: usize,
}

extern "C" {
    fn process_vm_readv(
        pid: c_int,
        local_iov: *const IoVec,
        liovcnt: c_ulong,
        remote_iov: *const IoVec,
        riovcnt: c_ulong,
        flags: c_ulong,
    ) -> isize;
}

pub struct LinuxBackend {
    pid: c_int,
}

impl LinuxBackend {
    pub fn new(pid: i32) -> Self {
        LinuxBackend { pid: pid as c_int }
    }
}

impl MemoryBackend for LinuxBackend {
    fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let local = IoVec {
            base: buf.as_mut_ptr() as *mut c_void,
            len: buf.len(),
        };
        let remote = IoVec {
            base: addr as *mut c_void,
            len: buf.len(),
        };
        let n = unsafe { process_vm_readv(self.pid, &local, 1, &remote, 1, 0) };
        if n < 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        let got = n as usize;
        if got != buf.len() {
            return Err(Error::ShortRead {
                expected: buf.len(),
                got,
            });
        }
        Ok(())
    }

    fn module_base(&self, name: &str) -> Result<u64> {
        // The load base is the lowest-addressed mapping backed by the module's
        // file. Parse /proc/<pid>/maps and take the minimum start among
        // mappings whose path basename matches.
        let maps = fs::read_to_string(format!("/proc/{}/maps", self.pid))?;
        let mut base: Option<u64> = None;
        for line in maps.lines() {
            // 55e0..-55e0.. r--p 00000000 08:01 1234  /path/to/module
            let mut cols = line.split_whitespace();
            let range = match cols.next() {
                Some(r) => r,
                None => continue,
            };
            let path = match cols.nth(4) {
                Some(p) => p,
                None => continue,
            };
            let matches = std::path::Path::new(path)
                .file_name()
                .map(|f| f == name)
                .unwrap_or(false);
            if !matches {
                continue;
            }
            if let Some(start_hex) = range.split('-').next() {
                if let Ok(start) = u64::from_str_radix(start_hex, 16) {
                    base = Some(base.map_or(start, |b| b.min(start)));
                }
            }
        }
        base.ok_or_else(|| Error::ModuleNotFound(name.to_string()))
    }

    fn readable_regions(&self) -> Result<Vec<Region>> {
        let maps = fs::read_to_string(format!("/proc/{}/maps", self.pid))?;
        let mut regions = Vec::new();
        for line in maps.lines() {
            let mut cols = line.split_whitespace();
            let range = match cols.next() {
                Some(r) => r,
                None => continue,
            };
            let perms = match cols.next() {
                Some(p) => p,
                None => continue,
            };
            if !perms.contains('r') {
                continue;
            }
            let (start_hex, end_hex) = match range.split_once('-') {
                Some(pair) => pair,
                None => continue,
            };
            if let (Ok(start), Ok(end)) = (
                u64::from_str_radix(start_hex, 16),
                u64::from_str_radix(end_hex, 16),
            ) {
                if end > start {
                    regions.push(Region {
                        start,
                        len: end - start,
                    });
                }
            }
        }
        Ok(regions)
    }
}
