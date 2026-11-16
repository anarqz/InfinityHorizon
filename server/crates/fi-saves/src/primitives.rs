//! Synchronization primitives of the in-memory store: std's normally, loom's when the crate is
//! built with `RUSTFLAGS="--cfg fi_loom"` so tests/loom.rs can model-check every interleaving.

#[cfg(fi_loom)]
pub(crate) use loom::sync::Mutex;
#[cfg(not(fi_loom))]
pub(crate) use std::sync::Mutex;
