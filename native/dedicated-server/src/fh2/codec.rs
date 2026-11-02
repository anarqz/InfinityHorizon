// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! NerdCore codec: to be written from docs/launcher/NERDCORE_WIRE.md (OD-187).
//!
//! Placeholder only. It declares what the engine scaffold calls and decodes nothing: every
//! datagram is `NotImplemented`, which the engine drops. Together with the engines admitting
//! nobody, the world stays fail closed (contract §2.2) until the codec is written from the spec.

/// Why a datagram was not decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// The codec is not written yet (OD-187): the datagram is dropped.
    NotImplemented,
}

/// A decoded NerdCore datagram. Uninhabited until the codec exists, so no code path can act on
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Datagram {}

/// Decodes one datagram. Always `Err(CodecError::NotImplemented)`.
pub fn decode(_data: &[u8]) -> Result<Datagram, CodecError> {
    Err(CodecError::NotImplemented)
}

/// Encodes one datagram. Unreachable until `Datagram` has variants.
pub fn encode(d: &Datagram, _out: &mut Vec<u8>) {
    match *d {}
}
