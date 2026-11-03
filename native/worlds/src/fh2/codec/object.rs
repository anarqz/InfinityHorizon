// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Object keys (§1.7), routers (§4.1), the object messages create/update/delete (§4.2-§4.5) and
//! the per-object sequence (§4.3).

use std::collections::BTreeMap;

use super::{
    CodecError,
    bits::{BitReader, BitRun, BitWriter, bit_length},
};

/// Width of the router-id field in object messages (§4.1).
pub const ROUTER_ID_BITS: u8 = 4;
/// Width of the per-object sequence (§4.3).
pub const SEQ_BITS: u8 = 8;
/// Sequence numbers count modulo 0xff, `0..=254` (§4.3).
pub const SEQ_MOD: u8 = 0xff;
/// Width of an object payload's length (§4.5).
pub const PAYLOAD_LEN_BITS: u8 = 14;
/// Longest payload in bits (`2^14 - 1`).
pub const PAYLOAD_MAX_BITS: usize = (1 << PAYLOAD_LEN_BITS) - 1;

/// Message ids of the object messages (§4.4).
pub const MSG_OBJECT_CREATE: u8 = 7;
pub const MSG_OBJECT_UPDATE: u8 = 8;
pub const MSG_OBJECT_DELETE: u8 = 9;

/// One router of §4.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouterSpec {
    pub id: u8,
    pub name: &'static str,
    pub max_objects: u32,
    /// The "second count" of §4.1 (an internal pool size; no known wire role).
    pub second_count: Option<u32>,
    pub types: u32,
}

impl RouterSpec {
    /// The object-id width: `bits(max_objects)` (§4.1).
    pub const fn id_bits(&self) -> u8 {
        bit_length(self.max_objects)
    }

    /// The type width: `bits(types)` (§4.1).
    pub const fn type_bits(&self) -> u8 {
        bit_length(self.types)
    }

    /// A create carries a type field only when the router has more than one type (§4.4).
    pub const fn has_type_field(&self) -> bool {
        self.types > 1
    }
}

/// The routers the client registers (§4.1). Ids 0, 4, 5, 7 and 13-15 are not registered.
#[rustfmt::skip]
pub const ROUTERS: [RouterSpec; 9] = [
    RouterSpec { id: 1, name: "IPlayerRouter", max_objects: 64, second_count: Some(12), types: 1 },
    RouterSpec { id: 2, name: "ICarRouter", max_objects: 64, second_count: None, types: 1 },
    RouterSpec { id: 3, name: "IConsoleManager", max_objects: 64, second_count: None, types: 1 },
    RouterSpec { id: 6, name: "ILobbyRouter", max_objects: 2, second_count: Some(1), types: 2 },
    RouterSpec { id: 8, name: "IClientTrafficRouter", max_objects: 1, second_count: None, types: 1 },
    RouterSpec { id: 9, name: "IRoadTripRouter", max_objects: 13, second_count: Some(1), types: 3 },
    RouterSpec { id: 10, name: "IWeatherRouter", max_objects: 1, second_count: None, types: 1 },
    RouterSpec { id: 11, name: "ICarMeetRouter", max_objects: 1, second_count: None, types: 1 },
    RouterSpec { id: 12, name: "IBucketListRouter", max_objects: 13, second_count: None, types: 2 },
];

/// The router with this id, if the client registers it.
pub fn router(id: u8) -> Option<&'static RouterSpec> {
    ROUTERS.iter().find(|r| r.id == id)
}

/// The kind bits of an object key (§1.7). Which owner kinds A, B and C denote is not
/// established by the spec (§4.2); the names follow the wire flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyKind {
    /// Flags `0, 0`: the receiver's own object.
    Own,
    /// Flags `1, 0` (in-memory `0x80000000`).
    A,
    /// Flags `0, 1` (in-memory `0x40000000`), followed by an 8-bit owner index.
    B { owner: u8 },
    /// Flags `1, 1` (in-memory `0x20000000`).
    C,
}

/// An object key (§1.7): kind plus an id of a field-specific width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectKey {
    pub kind: KeyKind,
    pub id: u32,
}

impl ObjectKey {
    /// Reads flag 1, flag 2, the `id_bits`-wide id, then the owner byte for kind B.
    pub fn read(r: &mut BitReader<'_>, id_bits: u8) -> Result<ObjectKey, CodecError> {
        let f1 = r.read_bool()?;
        let f2 = r.read_bool()?;
        let id = r.read(id_bits)?;
        let kind = match (f1, f2) {
            (false, false) => KeyKind::Own,
            (true, false) => KeyKind::A,
            (false, true) => KeyKind::B {
                owner: r.read(8)? as u8,
            },
            (true, true) => KeyKind::C,
        };
        Ok(ObjectKey { kind, id })
    }

    /// Writes the key; `id` must fit in `id_bits`.
    pub fn write(&self, w: &mut BitWriter, id_bits: u8) -> Result<(), CodecError> {
        let (f1, f2) = match self.kind {
            KeyKind::Own => (false, false),
            KeyKind::A => (true, false),
            KeyKind::B { .. } => (false, true),
            KeyKind::C => (true, true),
        };
        w.write_bool(f1)?;
        w.write_bool(f2)?;
        w.write(self.id, id_bits)?;
        if let KeyKind::B { owner } = self.kind {
            w.write(u32::from(owner), 8)?;
        }
        Ok(())
    }
}

/// An object message after its 7-bit id (§4.2, §4.4). Payloads are kept as their raw `L` bits
/// (§4.5); [`super::synced::ObjectSchema`] decodes them once a type's variable list is known.
///
/// The ownership and needs-updates bodies (10-13) follow the Infinity C# engine, which the D2
/// vectors confirm; NERDCORE_WIRE.md §4.4 leaves them undecoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectMessage {
    Create {
        router: u8,
        key: ObjectKey,
        /// 0 when the router has a single type (no type field on the wire).
        type_index: u32,
        seq: u8,
        payload: BitRun,
    },
    Update {
        router: u8,
        key: ObjectKey,
        seq: u8,
        payload: BitRun,
    },
    Delete {
        router: u8,
        key: ObjectKey,
        seq: u8,
    },
    /// 10: router, the type (only on routers with more than one type) *before* the key, then
    /// the key.
    RequestOwnership {
        router: u8,
        type_index: u32,
        key: ObjectKey,
    },
    /// 11: router, key, sequence, then a payload like an update's.
    GrantOwnership {
        router: u8,
        key: ObjectKey,
        seq: u8,
        payload: BitRun,
    },
    /// 12: router, key, sequence.
    TransferOwnership {
        router: u8,
        key: ObjectKey,
        seq: u8,
    },
    /// 13: ConsoleManager router (3) only: router, key, a 16-bit count, then that many 6-bit
    /// entries.
    NeedsUpdates {
        router: u8,
        key: ObjectKey,
        entries: Vec<u8>,
    },
}

/// Message ids of the ownership and needs-updates messages.
pub const MSG_REQUEST_OWNERSHIP: u8 = 10;
pub const MSG_GRANT_OWNERSHIP: u8 = 11;
pub const MSG_TRANSFER_OWNERSHIP: u8 = 12;
pub const MSG_NEEDS_UPDATES: u8 = 13;
/// The only router a NeedsUpdates may name.
pub const NEEDS_UPDATES_ROUTER: u8 = 3;
/// Width of a NeedsUpdates count and entry.
pub const NEEDS_UPDATES_COUNT_BITS: u8 = 16;
pub const NEEDS_UPDATES_ENTRY_BITS: u8 = 6;

fn read_payload(r: &mut BitReader<'_>) -> Result<BitRun, CodecError> {
    let len = r.read(PAYLOAD_LEN_BITS)? as usize;
    r.read_run(len)
}

fn write_payload(w: &mut BitWriter, p: &BitRun) -> Result<(), CodecError> {
    if p.len() > PAYLOAD_MAX_BITS {
        return Err(CodecError::Range);
    }
    w.write(p.len() as u32, PAYLOAD_LEN_BITS)?;
    w.write_run(p)
}

fn read_router(r: &mut BitReader<'_>) -> Result<&'static RouterSpec, CodecError> {
    let rid = r.read(ROUTER_ID_BITS)? as u8;
    router(rid).ok_or(CodecError::UnregisteredRouter(rid))
}

fn read_type(r: &mut BitReader<'_>, spec: &RouterSpec) -> Result<u32, CodecError> {
    if !spec.has_type_field() {
        return Ok(0);
    }
    let t = r.read(spec.type_bits())?;
    // SPEC: unverified (§4.4): a type index beyond the router's factories is treated as
    // invalid; neither the spec nor the references say what the game does with one.
    if t >= spec.types {
        return Err(CodecError::Range);
    }
    Ok(t)
}

fn write_type(w: &mut BitWriter, spec: &RouterSpec, t: u32) -> Result<(), CodecError> {
    if spec.has_type_field() {
        if t >= spec.types {
            return Err(CodecError::Range);
        }
        w.write(t, spec.type_bits())
    } else if t != 0 {
        Err(CodecError::Range)
    } else {
        Ok(())
    }
}

impl ObjectMessage {
    /// The message id (7 to 13).
    pub fn message_id(&self) -> u8 {
        match self {
            ObjectMessage::Create { .. } => MSG_OBJECT_CREATE,
            ObjectMessage::Update { .. } => MSG_OBJECT_UPDATE,
            ObjectMessage::Delete { .. } => MSG_OBJECT_DELETE,
            ObjectMessage::RequestOwnership { .. } => MSG_REQUEST_OWNERSHIP,
            ObjectMessage::GrantOwnership { .. } => MSG_GRANT_OWNERSHIP,
            ObjectMessage::TransferOwnership { .. } => MSG_TRANSFER_OWNERSHIP,
            ObjectMessage::NeedsUpdates { .. } => MSG_NEEDS_UPDATES,
        }
    }

    pub fn router(&self) -> u8 {
        match self {
            ObjectMessage::Create { router, .. }
            | ObjectMessage::Update { router, .. }
            | ObjectMessage::Delete { router, .. }
            | ObjectMessage::RequestOwnership { router, .. }
            | ObjectMessage::GrantOwnership { router, .. }
            | ObjectMessage::TransferOwnership { router, .. }
            | ObjectMessage::NeedsUpdates { router, .. } => *router,
        }
    }

    pub fn key(&self) -> ObjectKey {
        match self {
            ObjectMessage::Create { key, .. }
            | ObjectMessage::Update { key, .. }
            | ObjectMessage::Delete { key, .. }
            | ObjectMessage::RequestOwnership { key, .. }
            | ObjectMessage::GrantOwnership { key, .. }
            | ObjectMessage::TransferOwnership { key, .. }
            | ObjectMessage::NeedsUpdates { key, .. } => *key,
        }
    }

    /// The per-object sequence number, for the messages that carry one.
    pub fn seq(&self) -> Option<u8> {
        match self {
            ObjectMessage::Create { seq, .. }
            | ObjectMessage::Update { seq, .. }
            | ObjectMessage::Delete { seq, .. }
            | ObjectMessage::GrantOwnership { seq, .. }
            | ObjectMessage::TransferOwnership { seq, .. } => Some(*seq),
            ObjectMessage::RequestOwnership { .. } | ObjectMessage::NeedsUpdates { .. } => None,
        }
    }

    /// Reads the body of message `id` (7 to 13) after its 7-bit id.
    pub fn read(id: u8, r: &mut BitReader<'_>) -> Result<ObjectMessage, CodecError> {
        let spec = read_router(r)?;
        let router = spec.id;
        if id == MSG_REQUEST_OWNERSHIP {
            let type_index = read_type(r, spec)?;
            let key = ObjectKey::read(r, spec.id_bits())?;
            return Ok(ObjectMessage::RequestOwnership {
                router,
                type_index,
                key,
            });
        }
        if id == MSG_NEEDS_UPDATES && router != NEEDS_UPDATES_ROUTER {
            return Err(CodecError::UnregisteredRouter(router));
        }
        let key = ObjectKey::read(r, spec.id_bits())?;
        Ok(match id {
            MSG_OBJECT_CREATE => {
                let type_index = read_type(r, spec)?;
                let seq = r.read(SEQ_BITS)? as u8;
                let payload = read_payload(r)?;
                ObjectMessage::Create {
                    router,
                    key,
                    type_index,
                    seq,
                    payload,
                }
            }
            MSG_OBJECT_UPDATE | MSG_GRANT_OWNERSHIP => {
                let seq = r.read(SEQ_BITS)? as u8;
                let payload = read_payload(r)?;
                if id == MSG_OBJECT_UPDATE {
                    ObjectMessage::Update {
                        router,
                        key,
                        seq,
                        payload,
                    }
                } else {
                    ObjectMessage::GrantOwnership {
                        router,
                        key,
                        seq,
                        payload,
                    }
                }
            }
            MSG_OBJECT_DELETE | MSG_TRANSFER_OWNERSHIP => {
                let seq = r.read(SEQ_BITS)? as u8;
                if id == MSG_OBJECT_DELETE {
                    ObjectMessage::Delete { router, key, seq }
                } else {
                    ObjectMessage::TransferOwnership { router, key, seq }
                }
            }
            MSG_NEEDS_UPDATES => {
                let count = r.read(NEEDS_UPDATES_COUNT_BITS)? as usize;
                if count * usize::from(NEEDS_UPDATES_ENTRY_BITS) > r.remaining() {
                    return Err(CodecError::Truncated);
                }
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    entries.push(r.read(NEEDS_UPDATES_ENTRY_BITS)? as u8);
                }
                ObjectMessage::NeedsUpdates {
                    router,
                    key,
                    entries,
                }
            }
            other => return Err(CodecError::Unparsed(other)),
        })
    }

    /// Writes the body (without the 7-bit message id). Sequence 255 is written as given (the
    /// codec is lossless); `SeqCounter` never produces it and `ObjectSequencer` refuses it.
    pub fn write(&self, w: &mut BitWriter) -> Result<(), CodecError> {
        let rid = self.router();
        let spec = router(rid).ok_or(CodecError::UnregisteredRouter(rid))?;
        w.write(u32::from(rid), ROUTER_ID_BITS)?;
        if let ObjectMessage::RequestOwnership { type_index, .. } = self {
            write_type(w, spec, *type_index)?;
        }
        if matches!(self, ObjectMessage::NeedsUpdates { .. }) && rid != NEEDS_UPDATES_ROUTER {
            return Err(CodecError::UnregisteredRouter(rid));
        }
        self.key().write(w, spec.id_bits())?;
        match self {
            ObjectMessage::Create {
                type_index,
                seq,
                payload,
                ..
            } => {
                write_type(w, spec, *type_index)?;
                w.write(u32::from(*seq), SEQ_BITS)?;
                write_payload(w, payload)
            }
            ObjectMessage::Update { seq, payload, .. }
            | ObjectMessage::GrantOwnership { seq, payload, .. } => {
                w.write(u32::from(*seq), SEQ_BITS)?;
                write_payload(w, payload)
            }
            ObjectMessage::Delete { seq, .. } | ObjectMessage::TransferOwnership { seq, .. } => {
                w.write(u32::from(*seq), SEQ_BITS)
            }
            ObjectMessage::RequestOwnership { .. } => Ok(()),
            ObjectMessage::NeedsUpdates { entries, .. } => {
                let count = u32::try_from(entries.len()).map_err(|_| CodecError::Range)?;
                w.write(count, NEEDS_UPDATES_COUNT_BITS)?;
                entries
                    .iter()
                    .try_for_each(|e| w.write(u32::from(*e), NEEDS_UPDATES_ENTRY_BITS))
            }
        }
    }
}

/// `(s + 1) mod 0xff` (§4.3).
pub const fn seq_next(s: u8) -> u8 {
    seq_forward(s, 1)
}

/// The number `steps` places after `s`, modulo 0xff (§4.3). A wire value of 0xff counts as 0.
pub const fn seq_forward(s: u8, steps: u8) -> u8 {
    ((s as u16 + steps as u16) % SEQ_MOD as u16) as u8
}

/// Whether `candidate` is newer than `reference`: their difference wrapped to 8 bits is a
/// positive `i8` (1..=127). This is the relay's freshness test on raw 8-bit numbers (wire values
/// up to 0xff included); [`seq_order`] is the receiver's classification modulo 0xff and differs
/// from it at the half-way edge.
pub const fn seq_is_newer(candidate: u8, reference: u8) -> bool {
    (candidate.wrapping_sub(reference) as i8) > 0
}

/// How far `new` is ahead of `last`, modulo 0xff (both `< 0xff`).
pub const fn seq_ahead(new: u8, last: u8) -> u8 {
    ((new as u16 + SEQ_MOD as u16 - last as u16) % SEQ_MOD as u16) as u8
}

/// The sender side of §4.3: number each object's messages consecutively from its create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeqCounter {
    last: u8,
}

impl SeqCounter {
    /// Starts with the create's number (`< 0xff`).
    pub fn starting_at(create_seq: u8) -> SeqCounter {
        SeqCounter {
            last: create_seq % SEQ_MOD,
        }
    }

    /// The last number used.
    pub fn last(&self) -> u8 {
        self.last
    }

    /// The number for the next update or the delete.
    pub fn advance(&mut self) -> u8 {
        self.last = seq_next(self.last);
        self.last
    }
}

/// Where an incoming sequence number stands relative to the object's last applied one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqOrder {
    /// Exactly `(last + 1) mod 0xff`.
    Next,
    /// Ahead with a gap (held until the gap is filled).
    Ahead,
    /// The same number again.
    Same,
    /// Behind by 1..=127 ("old", dropped).
    Old,
}

/// Classifies `seq` against `last` (§4.3).
// SPEC: unverified (§4.3): the spec gives "newer" (held) and "older, behind by less than 128"
// (dropped); this splits the 255-value circle as ahead 2..=127 = held and ahead 128..=254
// (behind 1..=127) = old, and treats an equal number as a duplicate.
pub fn seq_order(seq: u8, last: u8) -> SeqOrder {
    match seq_ahead(seq, last) {
        0 => SeqOrder::Same,
        1 => SeqOrder::Next,
        2..=127 => SeqOrder::Ahead,
        _ => SeqOrder::Old,
    }
}

/// Outcome of an incoming create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateOutcome {
    Created,
    /// The object existed and the create is newer: it starts again from this number.
    Replaced,
    /// Older than (or the same as) the object's current number: dropped.
    DroppedOld,
    /// The table is at its limit.
    Full,
    /// Sequence number 255 (never produced by `mod 0xff`).
    InvalidSeq,
}

/// Outcome of an incoming update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome<T> {
    /// No such object: skipped (§4.4).
    Unknown,
    DroppedOld,
    /// Ahead with a gap: kept until the gap is filled.
    Held,
    /// Apply these in order: this update followed by any held ones it unblocked.
    Apply(Vec<T>),
    InvalidSeq,
}

/// Outcome of an incoming delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted,
    DroppedOld,
    Unknown,
    InvalidSeq,
}

#[derive(Debug, Clone)]
struct Tracked<T> {
    last: u8,
    held: Vec<(u8, T)>,
}

/// The receiver side of §4.3 for one connection: in-order application of updates, old creates and
/// deletes dropped. Bounded: at most `max_objects` objects, and per object at most 126 held
/// updates (one per distinct number ahead by 2..=127).
#[derive(Debug, Clone)]
pub struct ObjectSequencer<T> {
    objects: BTreeMap<(u8, ObjectKey), Tracked<T>>,
    max_objects: usize,
}

impl<T> ObjectSequencer<T> {
    pub fn new(max_objects: usize) -> ObjectSequencer<T> {
        ObjectSequencer {
            objects: BTreeMap::new(),
            max_objects,
        }
    }

    /// Objects tracked.
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// The object's last applied number.
    pub fn last(&self, router: u8, key: ObjectKey) -> Option<u8> {
        self.objects.get(&(router, key)).map(|t| t.last)
    }

    pub fn on_create(&mut self, router: u8, key: ObjectKey, seq: u8) -> CreateOutcome {
        if seq == SEQ_MOD {
            return CreateOutcome::InvalidSeq;
        }
        match self.objects.get_mut(&(router, key)) {
            Some(t) => match seq_order(seq, t.last) {
                // SPEC: unverified (§4.3): a newer create for an existing object restarts it.
                SeqOrder::Next | SeqOrder::Ahead => {
                    t.last = seq;
                    t.held.clear();
                    CreateOutcome::Replaced
                }
                SeqOrder::Same | SeqOrder::Old => CreateOutcome::DroppedOld,
            },
            None => {
                if self.objects.len() >= self.max_objects {
                    return CreateOutcome::Full;
                }
                self.objects.insert(
                    (router, key),
                    Tracked {
                        last: seq,
                        held: Vec::new(),
                    },
                );
                CreateOutcome::Created
            }
        }
    }

    pub fn on_update(&mut self, router: u8, key: ObjectKey, seq: u8, msg: T) -> UpdateOutcome<T> {
        if seq == SEQ_MOD {
            return UpdateOutcome::InvalidSeq;
        }
        let Some(t) = self.objects.get_mut(&(router, key)) else {
            return UpdateOutcome::Unknown;
        };
        match seq_order(seq, t.last) {
            SeqOrder::Same | SeqOrder::Old => UpdateOutcome::DroppedOld,
            SeqOrder::Ahead => {
                if !t.held.iter().any(|(s, _)| *s == seq) {
                    t.held.push((seq, msg));
                }
                UpdateOutcome::Held
            }
            SeqOrder::Next => {
                t.last = seq;
                let mut out = vec![msg];
                loop {
                    let want = seq_next(t.last);
                    match t.held.iter().position(|(s, _)| *s == want) {
                        Some(i) => {
                            let (s, m) = t.held.swap_remove(i);
                            t.last = s;
                            out.push(m);
                        }
                        None => break,
                    }
                }
                // anything now behind the window can never apply
                let last = t.last;
                t.held
                    .retain(|(s, _)| matches!(seq_order(*s, last), SeqOrder::Ahead));
                UpdateOutcome::Apply(out)
            }
        }
    }

    pub fn on_delete(&mut self, router: u8, key: ObjectKey, seq: u8) -> DeleteOutcome {
        if seq == SEQ_MOD {
            return DeleteOutcome::InvalidSeq;
        }
        let Some(t) = self.objects.get(&(router, key)) else {
            return DeleteOutcome::Unknown;
        };
        match seq_order(seq, t.last) {
            SeqOrder::Old => DeleteOutcome::DroppedOld,
            // SPEC: unverified (§4.3): a delete with the same number as the current one is applied
            // (the spec drops only older ones).
            SeqOrder::Same | SeqOrder::Next | SeqOrder::Ahead => {
                self.objects.remove(&(router, key));
                DeleteOutcome::Deleted
            }
        }
    }
}
