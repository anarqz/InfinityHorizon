//! Cloud saves v2 core (ADR 0005, docs/server/CLOUD_SAVES.md).
//!
//! The problem it solves: Beta players lost progress because two writers could overwrite each
//! other, a transient error at session start discarded a whole session, and nothing stopped a
//! delayed upload from replacing a newer save (Beta brief §8). The design:
//!
//! - [`lease`]: one writer per profile at a time, with **fencing tokens**; a writer that lost its
//!   lease can never write again, even if it does not know yet;
//! - [`store`]: every commit is a **compare-and-swap** on the head revision, idempotent by its key,
//!   decided and written in one critical section ([`memory`] here, Postgres in crates/fi-db);
//! - [`blob`]: bundles are **content-addressed and immutable**; the revision is committed only
//!   after its blob is stored;
//! - [`bundle`]: the FH2SAVE1 format and the **integrity checks** every upload passes;
//! - [`fm5`]: FM5's FM5SAVE1 format (a profile revision and its garage's sibling containers,
//!   CR-S105) and its integrity checks;
//! - [`fresh`]: the **fresh-start check** of a Hardcore line's first save from Azimuth (CR-S51);
//! - [`seed`]: the **seed check** of a Hardcore line started from another profile's save (CR-S52);
//! - [`gates`]: the **Hardcore progression gates**, on the line's play time, refusing loudly, never
//!   rolling back silently;
//! - [`reconcile`]: the **client decision table** (in sync, upload, download, conflict) the
//!   launcher runs before it starts the game;
//! - [`service`]: the use cases wiring them together;
//! - [`synthetic`]: made-up saves in the FH2SAVE1 and FM5SAVE1 shapes for every test (no
//!   game-derived bytes).
//!
//! The crate has no I/O and no runtime dependency, so the launcher (x86_64-pc-windows-gnu, checked
//! with `cargo check --target`) uses the same bundle, digest and decision code as the server.

pub mod blob;
pub mod bundle;
pub mod clock;
#[cfg(feature = "testkit")]
pub mod conformance;
pub mod error;
pub mod fm5;
pub mod fresh;
pub mod gates;
pub mod lease;
pub mod memory;
pub mod model;
mod primitives;
pub mod reconcile;
pub mod seed;
pub mod service;
pub mod store;
pub mod synthetic;

pub use blob::{BlobStore, MemoryBlobStore};
pub use error::SaveError;
pub use memory::{MemoryCore, MemorySaveStore};
pub use model::*;
pub use service::{
    AcceptParkedRequest, GuardrailsInput, InlineVerifier, ResetRequest, RestoreRequest, SaveFormat,
    SavePolicy, SaveService, UploadOutcome, UploadRequest, UploadVerdict, Verifier, guardrails,
    nightly_assessment,
};
pub use store::SaveStore;
