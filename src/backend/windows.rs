//! Windows backend: the production target.
//!
//! Reads a running game over the Win32 read APIs — `ReadProcessMemory`,
//! `VirtualQueryEx`, and module enumeration — declared directly against the
//! system libraries, matching the dependency-free style the Linux backend uses
//! against libc. The psapi routines are reached through their `K32`-prefixed
//! forwards in `kernel32`, so no import library beyond the default is needed.
//!
//! Like every backend it can only *read*: the process is opened with exactly
//! `PROCESS_QUERY_INFORMATION | PROCESS_VM_READ` — no write, no inject, no
//! execute rights — so the handle itself cannot mutate the target.
//!
//! NOTE ON TESTING: this module is type-checked by cross-compiling the library
//! to a Windows target, but it can only *run* on Windows against a real
//! process. The engine's platform-independent logic — the resolver, AOB
//! scanning, pointer-chain resolution, and PE build-id parsing ([`super::pe`])
//! — is what the Linux cavia tests actually exercise.

use std::ffi::c_void;
use std::io;
use std::ptr;

use crate::backend::{pe, MemoryBackend, Region};
use crate::error::{Error, Result};

type Handle = *mut c_void;
type Bool = i32;
type Dword = u32;

const PROCESS_QUERY_INFORMATION: Dword = 0x0400;
const PROCESS_VM_READ: Dword = 0x0010;

const MEM_COMMIT: Dword = 0x1000;
const PAGE_GUARD: Dword = 0x100;

const MAX_PATH: usize = 260;

#[allow(dead_code)] // fields mirror the OS struct; only `base_of_dll` is read.
#[repr(C)]
struct ModuleInfo {
    base_of_dll: *mut c_void,
    size_of_image: Dword,
    entry_point: *mut c_void,
}

// MEMORY_BASIC_INFORMATION. The pointer-sized members and the explicit
// alignment padding differ by the *reader's* bitness, so the layout is selected
// by target pointer width. A 64-bit reader inspecting a 32-bit (WOW64) target
// still uses the 64-bit layout — that is the supported cross-bitness case.
#[allow(dead_code)] // several members exist only to size the struct correctly.
#[cfg(target_pointer_width = "64")]
#[repr(C)]
struct MemoryBasicInformation {
    base_address: *mut c_void,
    allocation_base: *mut c_void,
    allocation_protect: Dword,
    alignment1: Dword,
    region_size: usize,
    state: Dword,
    protect: Dword,
    mem_type: Dword,
    alignment2: Dword,
}

#[allow(dead_code)]
#[cfg(target_pointer_width = "32")]
#[repr(C)]
struct MemoryBasicInformation {
    base_address: *mut c_void,
    allocation_base: *mut c_void,
    allocation_protect: Dword,
    region_size: usize,
    state: Dword,
    protect: Dword,
    mem_type: Dword,
}

extern "system" {
    fn OpenProcess(access: Dword, inherit: Bool, pid: Dword) -> Handle;
    fn CloseHandle(handle: Handle) -> Bool;
    fn IsWow64Process(handle: Handle, wow64: *mut Bool) -> Bool;
    fn ReadProcessMemory(
        handle: Handle,
        base: *const c_void,
        buffer: *mut c_void,
        size: usize,
        bytes_read: *mut usize,
    ) -> Bool;
    fn VirtualQueryEx(
        handle: Handle,
        address: *const c_void,
        info: *mut MemoryBasicInformation,
        length: usize,
    ) -> usize;
    fn K32EnumProcessModules(
        handle: Handle,
        modules: *mut Handle,
        cb: Dword,
        needed: *mut Dword,
    ) -> Bool;
    fn K32GetModuleBaseNameW(
        handle: Handle,
        module: Handle,
        base_name: *mut u16,
        size: Dword,
    ) -> Dword;
    fn K32GetModuleInformation(
        handle: Handle,
        module: Handle,
        info: *mut ModuleInfo,
        cb: Dword,
    ) -> Bool;
}

/// A read-only handle to a running Windows process.
pub struct WindowsBackend {
    handle: Handle,
    ptr_size: usize,
}

// SAFETY: the only non-`Send` field is `handle`, a raw process handle. A Windows
// HANDLE is a process-wide kernel object, not thread-affine: `ReadProcessMemory`
// and `VirtualQueryEx` may be called on it from any thread, and this backend only
// ever *reads* through it. Moving the backend to another thread — exactly what
// `Session::run` does to drive the polling loop in the background — is therefore
// sound. (No `Sync`: the backend is never shared across threads, only moved.)
unsafe impl Send for WindowsBackend {}

impl WindowsBackend {
    /// Open `pid` for reading only. Fails if the process cannot be opened —
    /// already gone, or the caller lacks the rights.
    pub fn open(pid: u32) -> Result<Self> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
        if handle.is_null() {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        let ptr_size = detect_pointer_size(handle);
        Ok(WindowsBackend { handle, ptr_size })
    }

    /// Find the module handle whose base name matches `name`, compared ASCII
    /// case-insensitively as the Windows filesystem does.
    fn find_module(&self, name: &str) -> Result<Handle> {
        let slot = std::mem::size_of::<Handle>();
        // Enumerate, growing the buffer if the OS reports it needs more room.
        let mut modules: Vec<Handle> = vec![ptr::null_mut(); 256];
        loop {
            let mut needed: Dword = 0;
            let cb = (modules.len() * slot) as Dword;
            let ok = unsafe {
                K32EnumProcessModules(self.handle, modules.as_mut_ptr(), cb, &mut needed)
            };
            if ok == 0 {
                return Err(Error::Io(io::Error::last_os_error()));
            }
            let needed_slots = needed as usize / slot;
            if needed_slots <= modules.len() {
                modules.truncate(needed_slots);
                break;
            }
            modules = vec![ptr::null_mut(); needed_slots];
        }

        for &module in &modules {
            let mut buf = [0u16; MAX_PATH];
            let len = unsafe {
                K32GetModuleBaseNameW(self.handle, module, buf.as_mut_ptr(), buf.len() as Dword)
            };
            if len == 0 {
                continue;
            }
            let got = String::from_utf16_lossy(&buf[..len as usize]);
            if got.eq_ignore_ascii_case(name) {
                return Ok(module);
            }
        }
        Err(Error::ModuleNotFound(name.to_string()))
    }
}

impl Drop for WindowsBackend {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

impl MemoryBackend for WindowsBackend {
    fn read_bytes(&self, addr: u64, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let mut read: usize = 0;
        let ok = unsafe {
            ReadProcessMemory(
                self.handle,
                addr as *const c_void,
                buf.as_mut_ptr() as *mut c_void,
                buf.len(),
                &mut read,
            )
        };
        if ok == 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        // A partial read is a failure here, exactly as on Linux: a half-resolved
        // pointer must never surface as a value.
        if read != buf.len() {
            return Err(Error::ShortRead {
                expected: buf.len(),
                got: read,
            });
        }
        Ok(())
    }

    fn module_base(&self, name: &str) -> Result<u64> {
        let module = self.find_module(name)?;
        let mut info = ModuleInfo {
            base_of_dll: ptr::null_mut(),
            size_of_image: 0,
            entry_point: ptr::null_mut(),
        };
        let ok = unsafe {
            K32GetModuleInformation(
                self.handle,
                module,
                &mut info,
                std::mem::size_of::<ModuleInfo>() as Dword,
            )
        };
        if ok == 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        Ok(info.base_of_dll as u64)
    }

    fn readable_regions(&self) -> Result<Vec<Region>> {
        let mut regions = Vec::new();
        let mut addr: u64 = 0;
        loop {
            let mut info: MemoryBasicInformation = unsafe { std::mem::zeroed() };
            let ret = unsafe {
                VirtualQueryEx(
                    self.handle,
                    addr as *const c_void,
                    &mut info,
                    std::mem::size_of::<MemoryBasicInformation>(),
                )
            };
            if ret == 0 {
                break; // walked off the top of the address space, or query failed
            }
            let base = info.base_address as u64;
            let size = info.region_size as u64;
            if size == 0 {
                break;
            }
            if info.state == MEM_COMMIT
                && (info.protect & PAGE_GUARD) == 0
                && is_readable_protect(info.protect)
            {
                regions.push(Region {
                    start: base,
                    len: size,
                });
            }
            // Advance past this region, stopping on wrap or lack of progress.
            match base.checked_add(size) {
                Some(next) if next > addr => addr = next,
                _ => break,
            }
        }
        Ok(regions)
    }

    fn module_version(&self, name: &str) -> Result<Option<String>> {
        let base = self.module_base(name)?;
        Ok(Some(pe::build_id(self, base)?))
    }

    fn pointer_size(&self) -> usize {
        self.ptr_size
    }
}

/// Pointer width of the *target*: 4 bytes for a 32-bit (WOW64) process, else the
/// reader's native width. A query failure falls back to native width rather than
/// guessing narrow.
fn detect_pointer_size(handle: Handle) -> usize {
    let mut wow64: Bool = 0;
    let ok = unsafe { IsWow64Process(handle, &mut wow64) };
    if ok != 0 && wow64 != 0 {
        4
    } else {
        std::mem::size_of::<usize>()
    }
}

/// True for page protections that permit reading, ignoring the guard/cache
/// modifier bits carried in the high byte. `PAGE_NOACCESS` (0x01) and
/// `PAGE_EXECUTE` (0x10, execute-only) are not readable.
fn is_readable_protect(protect: Dword) -> bool {
    matches!(protect & 0xFF, 0x02 | 0x04 | 0x08 | 0x20 | 0x40 | 0x80)
}
