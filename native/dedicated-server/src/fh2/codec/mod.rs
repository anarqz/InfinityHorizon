// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The NerdCore codec (docs/launcher/NERDCORE_WIRE.md):
//!
//! - [`bits`]: the bit stream (§1);
//! - [`packet`]: the 88-bit header, message dispatch and the voice trailer (§2);
//! - [`reliable`]: acknowledgements, loss detection and re-queueing as a sans-I/O state
//!   machine (§3);
//! - [`object`]: routers, object keys, create/update/delete and the per-object sequence (§1.7, §4);
//! - [`synced`]: synced-variable encodings and object payload variables (§4.5, §5);
//! - [`fields`]: the field notation of the message catalogue (§6.2);
//! - [`registry`]: the message-body registry keyed by message id, filled from §6.3.
//!
//! Pure and allocation-bounded: no I/O, no clock, no panics on hostile input.

pub mod bits;
pub mod fields;
pub mod object;
pub mod packet;
pub mod registry;
pub mod reliable;
pub mod synced;

pub use bits::{BitReader, BitRun, BitWriter};
pub use packet::{Message, Packet, PacketHeader, Tail};
pub use registry::Registry;

/// Why decoding or encoding failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// Fewer bits remain than the field needs.
    Truncated,
    /// A field width outside what the call allows (§1.3).
    Width(u8),
    /// A value that does not fit its field, or a count of 2^30 or more (§1.6).
    Range,
    /// A byte run or string longer than its field's maximum (§1.6).
    TooLong { len: usize, max: usize },
    /// The header's `size` is above 1372 bytes (§2.3), below the header, or past the datagram.
    Size(u16),
    /// A message id above 76 (§2.4).
    MessageId(u8),
    /// A message id that has no decodable body in the registry (or only ids 0-6 are accepted in
    /// connect-only mode, §2.4).
    Unparsed(u8),
    /// An object message names a router the client does not register (§4.1).
    Router(u8),
    /// An object payload's variables do not fill exactly its 14-bit length, or name a variable id
    /// the object type does not have (§4.5).
    Payload,
    /// A value given to an encoder does not match the message's layout.
    Layout,
    /// More than 255 messages in one packet (the header's count is 8 bits).
    TooManyMessages,
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for CodecError {}
