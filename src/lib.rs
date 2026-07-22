//! `scry` — a profile-driven, **read-only** game-memory telemetry engine.
//!
//! The crate's job is narrow on purpose: given a way to read another process's
//! memory (a [`MemoryBackend`]) and a description of where values live, produce
//! snapshots of those values. It knows nothing about streaming, clients, or any
//! particular host application — those live in whatever imports it.
//!
//! Everything the engine can do to a target is expressed through
//! [`MemoryBackend`], and every method on it only ever *reads*. There is no
//! write primitive anywhere in this crate, by design: the destructive half of a
//! tool like Cheat Engine simply does not exist here.

mod error;

pub mod aob;
pub mod backend;
pub mod engine;
pub mod profile;
pub mod resolver;

pub use backend::MemoryBackend;
pub use engine::{Config, Engine, Handle, Session, Snapshot, Value};
pub use error::{Error, Result};
pub use profile::{Match, Profile, Rip, ValueType, Watch};

#[cfg(target_os = "linux")]
pub use backend::linux::LinuxBackend;

#[cfg(target_os = "windows")]
pub use backend::windows::WindowsBackend;

/// The platform's production [`MemoryBackend`] for the current build target:
/// [`LinuxBackend`] on Linux (development and CI), [`WindowsBackend`] on Windows.
///
/// A convenience alias for *host* binaries — the CLI, the test cavia — that want
/// "the backend for this OS" without a per-platform `cfg` of their own. The
/// library itself stays host-agnostic: this only names a backend, it never
/// decides *what* to read. On a platform with no backend (e.g. macOS) the alias
/// simply does not exist, and a host must `cfg` around its absence.
#[cfg(target_os = "linux")]
pub type HostBackend = LinuxBackend;
#[cfg(target_os = "windows")]
pub type HostBackend = WindowsBackend;

/// Open the running process `pid` for read-only telemetry with the platform's
/// [`HostBackend`].
///
/// The single entry point a host needs to attach to a target on whatever OS it
/// was built for; everything above the backend seam (resolver, engine) is then
/// identical across platforms. Fallible because opening a process can be
/// refused (the target is gone, or the caller lacks the rights) — the honest
/// failure a host surfaces rather than guesses past.
#[cfg(target_os = "linux")]
pub fn open_host(pid: u32) -> Result<HostBackend> {
    Ok(LinuxBackend::new(pid as i32))
}
#[cfg(target_os = "windows")]
pub fn open_host(pid: u32) -> Result<HostBackend> {
    WindowsBackend::open(pid)
}
