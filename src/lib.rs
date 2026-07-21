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
pub use profile::{Match, Profile, ValueType, Watch};

#[cfg(target_os = "linux")]
pub use backend::linux::LinuxBackend;

#[cfg(target_os = "windows")]
pub use backend::windows::WindowsBackend;
