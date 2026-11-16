//! Content-addressed blob storage (CLOUD_SAVES.md §6).
//!
//! A bundle is stored once under its SHA-256 (`blobs/sha256/<hex>` in the `<project>-cloud-saves`
//! bucket) and never modified: the Cloud Storage adapter writes with `ifGenerationMatch=0`, so a
//! second write of the same digest is a no-op and no write can replace existing bytes. Readers
//! re-hash what they get back ([`crate::service::SaveService::download`]), so a corrupted object
//! is detected instead of being installed on a player's PC.
//!
//! Objects are shared: two profiles whose saves are byte-identical (every Hardcore fresh career of
//! one starter car) point at one object. Nothing but the two-phase garbage collection of
//! CLOUD_SAVES.md §6 ever deletes one; erasure deletes rows only.

use crate::error::SaveError;
use crate::model::Digest;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

/// Immutable, content-addressed byte storage.
pub trait BlobStore: Send + Sync {
    /// Stores `bytes` under `digest` if absent (an existing object is success). The caller has
    /// checked `digest == sha256(bytes)`.
    fn put(
        &self,
        digest: Digest,
        bytes: &[u8],
    ) -> impl Future<Output = Result<(), SaveError>> + Send;

    /// The bytes stored under `digest`.
    fn get(&self, digest: Digest) -> impl Future<Output = Result<Vec<u8>, SaveError>> + Send;
}

/// Process-memory blob store (tests, launcher preview).
#[derive(Clone, Default)]
pub struct MemoryBlobStore {
    blobs: Arc<Mutex<HashMap<Digest, Vec<u8>>>>,
}

impl MemoryBlobStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct blobs.
    pub fn len(&self) -> usize {
        self.blobs.lock().map(|b| b.len()).unwrap_or(0)
    }

    /// Whether no blob is stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Test control: overwrites a blob's bytes, as silent storage corruption would.
    pub fn corrupt(&self, digest: Digest, bytes: Vec<u8>) {
        if let Ok(mut blobs) = self.blobs.lock() {
            blobs.insert(digest, bytes);
        }
    }

    /// Test control: deletes an object, as garbage collection does.
    pub fn delete(&self, digest: Digest) {
        if let Ok(mut blobs) = self.blobs.lock() {
            blobs.remove(&digest);
        }
    }

    /// Whether an object is stored under `digest`.
    pub fn contains(&self, digest: Digest) -> bool {
        self.blobs
            .lock()
            .map(|b| b.contains_key(&digest))
            .unwrap_or(false)
    }
}

impl BlobStore for MemoryBlobStore {
    async fn put(&self, digest: Digest, bytes: &[u8]) -> Result<(), SaveError> {
        let mut blobs = self
            .blobs
            .lock()
            .map_err(|_| SaveError::Unavailable("blob store lock poisoned".into()))?;
        blobs.entry(digest).or_insert_with(|| bytes.to_vec());
        Ok(())
    }

    async fn get(&self, digest: Digest) -> Result<Vec<u8>, SaveError> {
        let blobs = self
            .blobs
            .lock()
            .map_err(|_| SaveError::Unavailable("blob store lock poisoned".into()))?;
        blobs
            .get(&digest)
            .cloned()
            .ok_or_else(|| SaveError::Unavailable(format!("blob {} missing", digest.hex())))
    }
}
