//! Offline **profile-authoring** tools.
//!
//! Everything here runs at *authoring time*, on files, to help a human produce a
//! [`Profile`](crate::profile::Profile) — never against a live process, never on
//! the polling hot path. That separation is deliberate: it is what lets the
//! runtime stay engine-agnostic (it reads plain offsets and signatures) while
//! the fiddly, engine-specific knowledge of *how a given engine lays out its
//! values* lives out here, compiled only under the `authoring` feature.
//!
//! Today the one tool is [`il2cpp`], a converter from Unity IL2CPP reflection
//! (as dumped by Il2CppDumper) to a scry profile. See
//! [`docs/authoring-il2cpp.md`](https://github.com/DanieleS/scry/blob/main/docs/authoring-il2cpp.md)
//! for the end-to-end workflow.

pub mod il2cpp;
