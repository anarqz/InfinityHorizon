// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The engine's view of an object key (NERDCORE_WIRE.md §1.7), on its own 64-bit reader. The
//! codec's [`ObjectKey`](crate::fh2::codec::object::ObjectKey) names the flag pairs neutrally; the
//! engine names them by who owns the object as the relay sees it.

use super::bits::{Bits, Rd, Rr};

/// Width of the owner index that follows a relayed key's id.
const OWNER_INDEX_BITS: u32 = 8;

/// Who owns the object a key names, from the key's two flag bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyOwner {
    /// Flags `0, 0`: the receiver's own object.
    Own,
    /// Flags `0, 1`: another peer's object, followed by that peer's owner index.
    Relay,
    /// Flags `1, 1`: a server-owned object.
    Server,
    /// Flags `1, 0`: an object in ownership transfer.
    Transfer,
}

impl KeyOwner {
    /// The `(first, second)` flag bits on the wire.
    const fn flags(self) -> (bool, bool) {
        match self {
            KeyOwner::Own => (false, false),
            KeyOwner::Relay => (false, true),
            KeyOwner::Server => (true, true),
            KeyOwner::Transfer => (true, false),
        }
    }
}

/// An object key as the engine keeps it; `peer` is the owner index of a relayed key, 0 otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjKey {
    pub kind: KeyOwner,
    pub id: u32,
    pub peer: u8,
}

impl ObjKey {
    const fn with(kind: KeyOwner, id: u32, peer: u8) -> ObjKey {
        ObjKey { kind, id, peer }
    }

    pub const fn own(id: u32) -> ObjKey {
        ObjKey::with(KeyOwner::Own, id, 0)
    }

    pub const fn relay(id: u32, peer: u8) -> ObjKey {
        ObjKey::with(KeyOwner::Relay, id, peer)
    }

    pub const fn transfer(id: u32) -> ObjKey {
        ObjKey::with(KeyOwner::Transfer, id, 0)
    }

    pub const fn server(id: u32) -> ObjKey {
        ObjKey::with(KeyOwner::Server, id, 0)
    }

    /// Reads the two flags, an `id_bits`-wide id and, for a relayed key, the owner index.
    pub fn read(r: &mut Rd<'_>, id_bits: u32) -> Rr<ObjKey> {
        let flags = (r.boolean()?, r.boolean()?);
        let id = r.read(id_bits)? as u32;
        Ok(match flags {
            (false, false) => ObjKey::own(id),
            (true, false) => ObjKey::transfer(id),
            (true, true) => ObjKey::server(id),
            (false, true) => ObjKey::relay(id, r.read(OWNER_INDEX_BITS)? as u8),
        })
    }

    /// Writes the key with an `id_bits`-wide id.
    pub fn write(&self, out: &mut Bits, id_bits: u32) {
        let (first, second) = self.kind.flags();
        out.boolean(first).boolean(second);
        out.push(u64::from(self.id), id_bits);
        if let KeyOwner::Relay = self.kind {
            out.push(u64::from(self.peer), OWNER_INDEX_BITS);
        }
    }
}
