// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Reliable delivery (§3) as a sans-I/O state machine: the caller supplies time and datagrams,
//! the state answers with ids to stamp, `ack`/`ackBits` to echo and messages to send again.
//!
//! Packet ids run `0..=0xfffe` and every distance is taken modulo 0xffff (§2.2, §3.3), never with
//! u16 wrapping.

use std::collections::VecDeque;

use super::packet::UNSEQUENCED;

/// The id space (§2.2).
pub const ID_MOD: u32 = 0xffff;
/// A sequenced packet this far or farther from the newest one (the short way round) is dropped
/// (§2.3).
pub const ID_WINDOW: u16 = 0x3fff;
/// "Within half the id space" (§3.2, §3.3).
pub const HALF_SPACE: u16 = 0x7fff;
/// Bits in `ackBits` (§2.1).
pub const ACK_BITS: u16 = 32;
/// The `ack` a side sends before it has received any sequenced packet, with `ackBits = 0`
/// (the C# engine and fh2-mods; §3.2 leaves the initial value unverified). `(0xfffe, 0)`
/// acknowledges nothing, and our own ids never use 0xfffe.
pub const NO_ACK: u16 = 0xfffe;
/// The timeout resend slack in milliseconds (§3.4: `rtt + 2000`).
pub const RESEND_SLACK_MS: u64 = 2000;

/// `(a - b) mod 0xffff` for ids below 0xffff.
pub const fn id_dist(a: u16, b: u16) -> u16 {
    ((a as u32 + ID_MOD - b as u32) % ID_MOD) as u16
}

/// `(prev + 1) mod 0xffff` (§2.2).
pub const fn id_next(prev: u16) -> u16 {
    ((prev as u32 + 1) % ID_MOD) as u16
}

/// What the receiver does with an incoming packet id (§2.3, §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvVerdict {
    /// `id == 0xffff`: no id check, nothing to acknowledge.
    Unsequenced,
    /// New: apply its messages; it will be acknowledged.
    Accepted,
    /// Already seen (equal to the newest id, or a bit already set in the window) or older than
    /// the 32-packet window: acknowledged but its messages are not applied again (the C# engine's
    /// rule; §2.3 drops an id equal to the newest).
    Duplicate,
    /// `0x3fff` or more away from the newest id: dropped silently (§2.3).
    Dropped,
}

/// The receive side: the newest sequenced id and the 32 ids before it (§3.2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReceiveState {
    newest: Option<u16>,
    bits: u32,
}

impl ReceiveState {
    pub fn new() -> ReceiveState {
        ReceiveState::default()
    }

    /// The newest accepted sequenced id, if any.
    pub fn newest(&self) -> Option<u16> {
        self.newest
    }

    /// Checks and records an incoming header id.
    pub fn accept(&mut self, id: u16) -> RecvVerdict {
        if id == UNSEQUENCED {
            return RecvVerdict::Unsequenced;
        }
        let Some(n) = self.newest else {
            // the first sequenced id is accepted whatever it is
            self.newest = Some(id);
            self.bits = 0;
            return RecvVerdict::Accepted;
        };
        if id == n {
            return RecvVerdict::Duplicate;
        }
        let ahead = id_dist(id, n);
        let behind = id_dist(n, id);
        if ahead.min(behind) >= ID_WINDOW {
            return RecvVerdict::Dropped;
        }
        if ahead > HALF_SPACE && (behind > ACK_BITS || self.bits & (1 << (behind - 1)) != 0) {
            return RecvVerdict::Duplicate;
        }
        if ahead <= HALF_SPACE {
            // newer: shift the window, the old newest becomes bit `ahead - 1`
            self.bits = if ahead >= ACK_BITS {
                0
            } else {
                self.bits << ahead
            };
            if ahead <= ACK_BITS {
                self.bits |= 1 << (ahead - 1);
            }
            self.newest = Some(id);
        } else if (1..=ACK_BITS).contains(&behind) {
            self.bits |= 1 << (behind - 1);
        }
        RecvVerdict::Accepted
    }

    /// `(ack, ackBits)` to put in every outgoing packet (§3.2, §3.5). Before any sequenced
    /// packet: `(NO_ACK, 0)`.
    pub fn ack(&self) -> (u16, u32) {
        (self.newest.unwrap_or(NO_ACK), self.bits)
    }
}

/// A sent sequenced packet still waiting for its acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending<M> {
    pub id: u16,
    pub sent_ms: u64,
    /// The packet's resendable messages (class below 3).
    pub messages: Vec<M>,
}

/// The result of an incoming `ack` / `ackBits`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckOutcome<M> {
    /// Acknowledged packets, their reliable messages done.
    pub acked: Vec<Pending<M>>,
    /// Ids of packets found lost; their messages are now on the re-queue.
    pub lost: Vec<u16>,
}

/// The send side (§3.3-§3.5): numbering, pending packets, loss detection, re-queue.
#[derive(Debug, Clone)]
pub struct SendState<M> {
    last_id: u16,
    pending: VecDeque<Pending<M>>,
    requeue: VecDeque<M>,
    max_pending: usize,
}

impl<M> SendState<M> {
    /// `max_pending` bounds the packets kept for acknowledgement; past it the oldest is treated
    /// as lost.
    pub fn new(max_pending: usize) -> SendState<M> {
        SendState {
            // the first sequenced id is 1, as in the C# engine (an id 0 could read as acknowledged
            // by a peer's initial `ack` of 0, §3.2)
            last_id: 0,
            pending: VecDeque::new(),
            requeue: VecDeque::new(),
            max_pending: max_pending.max(1),
        }
    }

    /// The id for the next packet that carries a reliable message (§3.5): `(last + 1) mod 0xffff`,
    /// skipping [`NO_ACK`] (the C# engine wraps from 0xfffd to 0) so that our echo of a peer's
    /// `(0xfffe, 0)` can never be mistaken for an acknowledgement.
    pub fn next_id(&mut self) -> u16 {
        self.last_id = id_next(self.last_id);
        if self.last_id == NO_ACK {
            self.last_id = id_next(self.last_id);
        }
        self.last_id
    }

    pub fn pending(&self) -> impl Iterator<Item = &Pending<M>> {
        self.pending.iter()
    }

    /// Records a sent sequenced packet and its resendable messages.
    pub fn on_sent(&mut self, id: u16, now_ms: u64, messages: Vec<M>) {
        if id == UNSEQUENCED {
            return;
        }
        while self.pending.len() >= self.max_pending {
            if let Some(p) = self.pending.pop_front() {
                self.requeue.extend(p.messages);
            }
        }
        self.pending.push_back(Pending {
            id,
            sent_ms: now_ms,
            messages,
        });
    }

    /// Processes a peer's `ack` and `ackBits` (§3.3).
    pub fn on_ack(&mut self, ack: u16, ack_bits: u32) -> AckOutcome<M> {
        let mut out = AckOutcome {
            acked: Vec::new(),
            lost: Vec::new(),
        };
        // `(0xfffe, 0)` is a peer that has received nothing yet (the C# engine's rule).
        // SPEC: unverified (§3.3): an `ack` of 0xffff (never a sequence number) is ignored too.
        if ack == UNSEQUENCED || (ack == NO_ACK && ack_bits == 0) {
            return out;
        }
        let mut keep = VecDeque::with_capacity(self.pending.len());
        for p in self.pending.drain(..) {
            let d = id_dist(ack, p.id);
            let acked = d == 0 || ((1..=ACK_BITS).contains(&d) && ack_bits & (1 << (d - 1)) != 0);
            if acked {
                out.acked.push(p);
            } else if (1..=HALF_SPACE).contains(&id_dist(p.id, ack)) {
                keep.push_back(p);
            } else {
                out.lost.push(p.id);
                self.requeue.extend(p.messages);
            }
        }
        self.pending = keep;
        out
    }

    /// Timeout resend (§3.4): packets sent more than `rtt + 2000` ms ago are lost. Returns their
    /// ids; their messages go to the re-queue.
    pub fn on_timer(&mut self, now_ms: u64, rtt_ms: u64) -> Vec<u16> {
        let limit = rtt_ms.saturating_add(RESEND_SLACK_MS);
        let mut lost = Vec::new();
        let mut keep = VecDeque::with_capacity(self.pending.len());
        for p in self.pending.drain(..) {
            if now_ms.saturating_sub(p.sent_ms) > limit {
                lost.push(p.id);
                self.requeue.extend(p.messages);
            } else {
                keep.push_back(p);
            }
        }
        self.pending = keep;
        lost
    }

    /// Messages to send again, oldest first.
    pub fn take_requeue(&mut self) -> Vec<M> {
        self.requeue.drain(..).collect()
    }

    pub fn requeue_len(&self) -> usize {
        self.requeue.len()
    }
}
