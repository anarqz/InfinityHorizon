// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Packets out: unsequenced messages and acknowledgements, the reliable path (a window of
//! in-flight packets, a bounded queue behind it, pacing, resends under a new packet id, 20 tries),
//! the pre-sync amplification guard (never more bytes to an unsynced peer than it sent), the
//! datagram limit, drops, and the engine tick.

use std::sync::Arc;

use serde_json::json;

use super::{
    Engine, Event, M, Peer, Reliable, Why,
    bits::Bits,
    wire::{self, Handle, Msg, NO_SEQUENCE, PACKET_BODY_SPLIT, seq_add, seq_next},
};

/// The peer goes at the end of this receive or tick; the first reason is kept.
pub(super) fn doom(p: &mut Peer, why: Why) {
    if p.doomed_by.is_none() {
        p.doomed_by = Some(why);
    }
    p.doomed = true;
}

/// A sender's reliable fan-out budget: messages its own traffic makes the server send reliably
/// to the others (forwarded objects, Ready and score rebroadcasts, votes), counted once per
/// message whatever the number of receivers. A burst of `FANOUT_BURST`, refilled at
/// `FANOUT_PER_SECOND`; past it the SENDER is dropped (`peer.dropped.fan_out`) before its
/// flood fills, and dooms, every receiver's reliable queue. A receiver that stops acknowledging
/// is still dropped by its own full queue (the reference's rule).
pub const FANOUT_BURST: f64 = 1024.0;
pub const FANOUT_PER_SECOND: f64 = 256.0;

/// At most this many messages share one packet.
const MESSAGES_PER_PACKET: usize = 8;
/// What one pacing turn sends per peer at most (bytes).
const PACE_BUDGET: usize = 8 * 1024;
/// A reliable packet is resent this many times before the peer is dropped.
const MAX_TRIES: u32 = 20;

impl Engine {
    pub(super) fn doom(&mut self, serial: u64, why: Why) {
        if let Some(p) = self.peer_mut(serial) {
            doom(p, why);
        }
    }

    /// Whether anything may go to the peer; a peer no longer allowed is doomed.
    pub(super) fn may_send(&mut self, serial: u64) -> bool {
        let Some(p) = self.peer(serial) else {
            return false;
        };
        let (doomed, xuid, endpoint) = (p.doomed, p.xuid, p.endpoint);
        if !doomed && self.allowed(xuid, endpoint) {
            return true;
        }
        self.doom(serial, Why::NotAllowedAnyMore);
        false
    }

    pub(super) fn send(&mut self, serial: u64, messages: Vec<M>, sequenced: bool, tries: u32) {
        if !self.may_send(serial) {
            return;
        }
        // a held joiner's first copy only (a resend repeats what already passed)
        let messages = if tries == 0 {
            let m = self.unheld(serial, messages);
            if m.is_empty() {
                return;
            }
            m
        } else {
            messages
        };
        let mut chunk: Vec<M> = Vec::new();
        let mut bits = 0usize;
        for m in messages {
            if m.len() > wire::MAX_MESSAGE_BITS {
                // a message that alone cannot fit a datagram is dropped, never sent
                self.oversize_dropped += 1;
                let id = m.reader().read(7).unwrap_or(0);
                let Some(p) = self.peer_mut(serial) else {
                    continue;
                };
                p.oversize_dropped += 1;
                let (index, dropped) = (p.index, p.oversize_dropped);
                if self.peer_note(serial, "oversize") {
                    self.note(
                        "send.oversize_dropped",
                        vec![
                            json!(index),
                            json!(id),
                            json!(m.len()),
                            json!(wire::MAX_DATAGRAM),
                            json!(dropped),
                        ],
                    );
                }
                continue;
            }
            if !chunk.is_empty()
                && (chunk.len() >= MESSAGES_PER_PACKET
                    || (bits + m.len()).div_ceil(8) > PACKET_BODY_SPLIT)
            {
                self.flush(serial, std::mem::take(&mut chunk), sequenced, tries);
                bits = 0;
            }
            bits += m.len();
            chunk.push(m);
        }
        self.flush(serial, chunk, sequenced, tries);
    }

    fn flush(&mut self, serial: u64, chunk: Vec<M>, sequenced: bool, tries: u32) {
        if chunk.is_empty() {
            return;
        }
        let window = self.settings.relay.reliable_window as usize;
        let Some(p) = self.peer(serial) else { return };
        if sequenced && p.in_flight.len() >= window {
            for m in chunk {
                if !self.queue_reliable(serial, m) {
                    break;
                }
            }
            return;
        }
        let now = self.now;
        let Some(p) = self.peer_mut(serial) else {
            return;
        };
        let (ack, ack_bits) = p.acks.header();
        let id = if sequenced {
            p.take_packet_id()
        } else {
            NO_SEQUENCE
        };
        let packet = wire::packet(chunk.iter().map(|m| m.as_ref()), id, ack, ack_bits);
        if p.synced || p.sent_bytes + packet.len() as i64 <= p.received_bytes {
            if !p.synced {
                p.sent_bytes += packet.len() as i64;
            }
            let endpoint = p.endpoint;
            if sequenced {
                p.in_flight.push(Reliable {
                    id,
                    messages: chunk,
                    sent_at: now,
                    tries,
                });
            }
            self.emit(endpoint, packet);
        }
    }

    /// Answers (or a bare acknowledgement) to the peer.
    pub(super) fn send_ack(&mut self, serial: u64, messages: Vec<M>) {
        if !self.may_send(serial) {
            return;
        }
        if !messages.is_empty() {
            self.send(serial, messages, false, 0);
            return;
        }
        let Some(p) = self.peer_mut(serial) else {
            return;
        };
        let (ack, bits) = p.acks.header();
        let packet = wire::packet(std::iter::empty::<&Bits>(), NO_SEQUENCE, ack, bits);
        if !p.synced && p.sent_bytes + packet.len() as i64 > p.received_bytes {
            return;
        }
        if !p.synced {
            p.sent_bytes += packet.len() as i64;
        }
        let endpoint = p.endpoint;
        self.emit(endpoint, packet);
    }

    pub(super) fn send_sequenced(&mut self, serial: u64, messages: Vec<M>) {
        if !self.may_send(serial) {
            return;
        }
        // nothing held enters the queue, so a release never finds a stale copy there
        let messages = self.unheld(serial, messages);
        if messages.is_empty() {
            return;
        }
        if self.peer(serial).is_some_and(|p| p.queue.is_empty()) {
            self.send(serial, messages, true, 0);
            return;
        }
        for m in messages {
            if !self.queue_reliable(serial, m) {
                return;
            }
        }
    }

    pub(super) fn queue_reliable(&mut self, serial: u64, m: M) -> bool {
        let max_messages = self.settings.relay.queue_max_messages as usize;
        let max_bits = self.settings.relay.queue_max_bits as usize;
        let Some(p) = self.peer_mut(serial) else {
            return false;
        };
        if p.queue.len() >= max_messages || p.queue_bits + m.len() > max_bits {
            doom(p, Why::QueueFull);
            return false;
        }
        p.queue_bits += m.len();
        p.queue.push_back(m);
        true
    }

    pub(super) fn pace(&mut self, serial: u64) {
        let window = self.settings.relay.reliable_window as usize;
        let mut budget = PACE_BUDGET as i64;
        loop {
            let Some(p) = self.peer_mut(serial) else {
                return;
            };
            if !(p.in_flight.len() < window && !p.queue.is_empty() && budget > 0) {
                return;
            }
            let mut batch = Vec::new();
            let mut bits = 0usize;
            while batch.len() < MESSAGES_PER_PACKET
                && let Some(m) = p.queue.front()
            {
                if !batch.is_empty() && (bits + m.len()).div_ceil(8) > PACKET_BODY_SPLIT {
                    break;
                }
                bits += m.len();
                p.queue_bits -= m.len();
                if let Some(m) = p.queue.pop_front() {
                    batch.push(m);
                }
            }
            budget -= bits.div_ceil(8) as i64;
            self.send(serial, batch, true, 0);
            if self.peer(serial).is_none_or(|p| p.doomed) {
                return;
            }
        }
    }

    /// Charges `n` reliable fan-out messages to `sender`; `false` (and the sender doomed) past
    /// its budget.
    pub(super) fn charge_fanout(&mut self, sender: u64, n: usize) -> bool {
        let now = self.now;
        let (tokens, at) = self
            .fanout
            .get(&sender)
            .copied()
            .unwrap_or((FANOUT_BURST, now));
        let refilled =
            (tokens + (now - at).max(0) as f64 * FANOUT_PER_SECOND / 1000.0).min(FANOUT_BURST);
        let left = refilled - n as f64;
        if left < 0.0 {
            self.fanout.insert(sender, (0.0, now));
            self.doom(sender, Why::FanOut);
            return false;
        }
        self.fanout.insert(sender, (left, now));
        true
    }

    /// The same messages to every other synced peer.
    pub(super) fn forward(&mut self, from: u64, messages: Vec<M>, sequenced: bool) {
        if messages.is_empty() {
            return;
        }
        let receivers = self.synced_serials_except(Some(from));
        if sequenced && !receivers.is_empty() && !self.charge_fanout(from, messages.len()) {
            return;
        }
        for s in receivers {
            if sequenced {
                self.send_sequenced(s, messages.clone());
            } else {
                self.send(s, messages.clone(), false, 0);
            }
        }
    }

    /// The peer leaves this engine; the world log gets its index and why.
    pub(super) fn drop_peer(&mut self, serial: u64, why: Why) {
        let Some(at) = self.pos(serial) else { return };
        let p = self.peers.remove(at);
        self.holding.remove(&serial);
        self.cars_pending.remove(&serial);
        let mut args = vec![json!(p.index)];
        args.extend(why.args());
        args.push(json!(p.synced));
        self.note(why.id(), args);
        let deletes: Vec<M> = p
            .objects
            .iter()
            .map(|(key, obj)| {
                Arc::new(wire::write(&Msg::Delete {
                    router: key.0,
                    handle: Handle::relay(key.1, p.index),
                    seq: seq_add(seq_next(obj.last), obj.injected),
                }))
            })
            .collect();
        for s in self.synced_serials_except(None) {
            self.send_sequenced(s, deletes.clone());
        }
        self.side_rates.remove(&serial);
        self.fanout.remove(&serial);
        self.voice_peer_left(serial);
        self.watch_peer_left(p.index);
        if self.peers.is_empty() {
            self.group_node = 0;
        }
        self.events.push(Event::PeerLeft {
            xuid: p.xuid,
            peer: p.index,
            why,
            synced: p.synced,
        });
        self.peer_left_seams(&p);
    }

    pub(super) fn drop_doomed(&mut self) {
        let doomed: Vec<u64> = self
            .peers
            .iter()
            .filter(|p| p.doomed)
            .map(|p| p.serial)
            .collect();
        for s in doomed {
            let Some(p) = self.peer(s) else { continue };
            let (synced, xuid, endpoint) = (p.synced, p.xuid, p.endpoint);
            if synced && self.allowed(xuid, endpoint) {
                let Some(p) = self.peer(s) else { continue };
                let (ack, bits) = p.acks.header();
                let close = wire::write(&Msg::Close);
                self.emit(endpoint, wire::packet([&close], NO_SEQUENCE, ack, bits));
            }
            let why = self
                .peer(s)
                .and_then(|p| p.doomed_by)
                .unwrap_or(Why::Doomed);
            self.drop_peer(s, why);
        }
    }

    /// The handshake's last step: the time and SyncDone, unsequenced, repeated until confirmed.
    pub(super) fn send_completion(&mut self, serial: u64) {
        let now = self.now;
        let wire_now = self.now_wire();
        let Some(p) = self.peer_mut(serial) else {
            return;
        };
        p.last_completion = now;
        p.last_time_sync = now;
        let index = p.sync_index;
        p.sync_index = p.sync_index.wrapping_add(1);
        let attempt = p.attempt;
        let messages = vec![
            Arc::new(wire::write(&Msg::TimeSync {
                index,
                time: wire_now,
            })),
            Arc::new(wire::write(&Msg::SyncDone { attempt })),
        ];
        self.send(serial, messages, false, 0);
    }

    /// The 20 ms engine tick.
    pub fn tick(&mut self, now_ms: i64) {
        if self.closed {
            return;
        }
        self.now = self.now.max(now_ms);
        self.tick_held_opens();
        self.tick_quiet_joins();
        self.tick_side_channels();
        self.tick_race();
        self.tick_game_options();
        self.tick_phasing();
        self.tick_weather();
        self.tick_bucket_list();
        self.tick_car_meet();
        self.tick_road_trip();
        self.tick_watch();
        let now = self.now;
        let r = self.settings.relay.clone();
        if now - self.last_resend >= 50 {
            self.last_resend = now;
            let serials: Vec<u64> = self.peers.iter().map(|p| p.serial).collect();
            for s in serials {
                let Some(p) = self.peer(s) else { continue };
                if p.synced && !p.completion_confirmed {
                    if now - p.joined_at >= r.sync_complete_ms as i64 {
                        self.doom(s, Why::SyncUnconfirmed);
                        continue;
                    }
                    if now - p.last_completion >= r.resend_after_ms as i64 {
                        self.send_completion(s);
                    }
                }
                let Some(p) = self.peer_mut(s) else { continue };
                let wait = (r.resend_after_ms as i64).max(2 * i64::from(p.rtt.unwrap_or(0)));
                let mut due = Vec::new();
                p.in_flight.retain(|x| {
                    if now - x.sent_at >= wait {
                        due.push(x.clone());
                        false
                    } else {
                        true
                    }
                });
                for packet in due {
                    if packet.tries >= MAX_TRIES {
                        self.doom(s, Why::Unacknowledged(MAX_TRIES));
                    } else {
                        self.send(s, packet.messages, true, packet.tries + 1);
                    }
                }
            }
        }
        if now - self.last_pace >= 20 {
            self.last_pace = now;
            let serials: Vec<u64> = self.peers.iter().map(|p| p.serial).collect();
            for s in serials {
                self.pace(s);
            }
        }
        if now - self.last_keepalive >= r.keepalive_ms as i64 {
            self.last_keepalive = now;
            let serials: Vec<u64> = self.peers.iter().map(|p| p.serial).collect();
            for s in serials {
                let Some(p) = self.peer(s) else { continue };
                let (xuid, endpoint, last_seen, synced, joined_at, last_time_sync) = (
                    p.xuid,
                    p.endpoint,
                    p.last_seen,
                    p.synced,
                    p.joined_at,
                    p.last_time_sync,
                );
                if !self.allowed(xuid, endpoint) {
                    self.drop_peer(s, Why::NotAllowedAnyMore);
                } else if now - last_seen > r.quiet_drop_ms as i64 {
                    self.drop_peer(s, Why::Quiet(now - last_seen));
                } else if !synced && now - joined_at > r.unsynced_timeout_ms as i64 {
                    self.drop_peer(s, Why::NeverSynced);
                } else if synced {
                    let wire_now = self.now_wire();
                    let ping = Arc::new(wire::write(&Msg::Ping { time: wire_now }));
                    if now - last_time_sync >= r.time_sync_ms as i64 {
                        let Some(p) = self.peer_mut(s) else { continue };
                        p.last_time_sync = now;
                        let index = p.sync_index;
                        p.sync_index = p.sync_index.wrapping_add(1);
                        let ts = Arc::new(wire::write(&Msg::TimeSync {
                            index,
                            time: wire_now,
                        }));
                        self.send(s, vec![ping, ts], false, 0);
                    } else {
                        self.send(s, vec![ping], false, 0);
                    }
                }
            }
        }
        self.drop_doomed();
    }
}
