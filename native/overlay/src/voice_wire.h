// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the world relay's voice side channel (OD-154, D1; docs/launcher/VOICE_CHAT.md §3.1, §12.3). The
// launcher's Infinity.Core/WorldServer/VoiceCodec.cs is the reference: the same golden vectors in both test suites.
// Header only, no Windows, so it is tested on its own.
//
// STUN indications (RFC 5389 header, magic 0x2112A442, one attribute, 4-byte padding), at most 548 bytes, big-endian:
//   0x3E93 voice: transaction id {ver = 1, flags, seq u16, ts u32 (20 ms units), up: 4 random | down: seat, 3 random};
//          attribute 0xC0F3 (up) / 0xC0F4 (down) = one Opus packet, 1-400 bytes
//   0x3E94 control up, attribute 0xC0F5: {ver, op (1 subscribe, 2 replace the mutes, 3 leave), xuid u64, self (bit 0
//          mic muted, bit 1 deafened), count <= 12, count x xuid u64}
//   0x3E94 state down, attribute 0xC0F6: {ver, lobby (bit 0 voice on), bundle (3 or 6), count <= 12, count x {seat,
//          xuid u64, state (bit 0 talking, 1 muted for the lobby, 2 blocked)}}
#pragma once

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <vector>

namespace fi::voice_wire {

constexpr uint16_t kVoiceKind = 0x3E93, kControlKind = 0x3E94;
constexpr uint16_t kVoiceUp = 0xC0F3, kVoiceDown = 0xC0F4, kControlUp = 0xC0F5, kStateDown = 0xC0F6;
constexpr uint8_t kVersion = 1;
constexpr size_t kMaxDatagram = 548, kMaxOpus = 400, kMaxSeats = 12;
constexpr uint32_t kMagic = 0x2112A442;

enum : uint8_t { kSubscribe = 1, kReplaceMutes = 2, kLeave = 3 };
enum : uint8_t { kSelfMicMuted = 1, kSelfDeafened = 2 };
enum : uint8_t { kSeatTalking = 1, kSeatMutedForLobby = 2, kSeatBlocked = 4 };
enum : uint8_t { kEndOfSpurt = 1, kPushToTalk = 2 };

inline void Be16(uint8_t *p, uint16_t v) { p[0] = static_cast<uint8_t>(v >> 8); p[1] = static_cast<uint8_t>(v); }
inline void Be32(uint8_t *p, uint32_t v) { Be16(p, static_cast<uint16_t>(v >> 16)); Be16(p + 2, static_cast<uint16_t>(v)); }
inline void Be64(uint8_t *p, uint64_t v) { Be32(p, static_cast<uint32_t>(v >> 32)); Be32(p + 4, static_cast<uint32_t>(v)); }
inline uint16_t Rd16(const uint8_t *p) { return static_cast<uint16_t>(p[0] << 8 | p[1]); }
inline uint32_t Rd32(const uint8_t *p) { return static_cast<uint32_t>(Rd16(p)) << 16 | Rd16(p + 2); }
inline uint64_t Rd64(const uint8_t *p) { return static_cast<uint64_t>(Rd32(p)) << 32 | Rd32(p + 4); }

// A STUN indication with one attribute; empty when it would pass 548 bytes.
inline std::vector<uint8_t> Indication(uint16_t kind, uint16_t attribute, const uint8_t *value, size_t size, const uint8_t transaction[12]) {
    const size_t length = 24 + (size + 3) / 4 * 4;
    if (length > kMaxDatagram) return {};
    std::vector<uint8_t> out(length, 0);
    Be16(out.data(), kind);
    Be16(out.data() + 2, static_cast<uint16_t>(length - 20));
    Be32(out.data() + 4, kMagic);
    std::memcpy(out.data() + 8, transaction, 12);
    Be16(out.data() + 20, attribute);
    Be16(out.data() + 22, static_cast<uint16_t>(size));
    if (size) std::memcpy(out.data() + 24, value, size);
    return out;
}

// The attribute's value when `data` is a well-formed indication of `kind` carrying `attribute`.
inline bool Attribute(const uint8_t *data, size_t size, uint16_t kind, uint16_t attribute, const uint8_t **value, size_t *value_size) {
    if (size < 20 || size > kMaxDatagram || size % 4 || (data[0] & 0xC0) || Rd32(data + 4) != kMagic || Rd16(data + 2) + 20u != size || Rd16(data) != kind)
        return false;
    for (size_t at = 20; at + 4 <= size;) {
        const uint16_t tag = Rd16(data + at), count = Rd16(data + at + 2);
        if (count > size - at - 4) return false;
        if (tag == attribute) {
            *value = data + at + 4;
            *value_size = count;
            return true;
        }
        at += 4 + (count + 3u) / 4 * 4;
    }
    return false;
}

// Voice up: `random` is the transaction id's last 4 bytes (random in play, fixed in the golden vectors).
inline std::vector<uint8_t> EncodeVoiceUp(uint8_t flags, uint16_t seq, uint32_t ts, const uint8_t *opus, size_t size, const uint8_t random[4]) {
    if (size == 0 || size > kMaxOpus) return {};
    uint8_t transaction[12] = {kVersion, flags};
    Be16(transaction + 2, seq);
    Be32(transaction + 4, ts);
    std::memcpy(transaction + 8, random, 4);
    return Indication(kVoiceKind, kVoiceUp, opus, size, transaction);
}

struct VoiceDown {
    uint8_t flags = 0, seat = 0;
    uint16_t seq = 0;
    uint32_t ts = 0;
    const uint8_t *opus = nullptr; // into the datagram
    size_t size = 0;
};

inline bool DecodeVoiceDown(const uint8_t *data, size_t size, VoiceDown &out) {
    const uint8_t *value = nullptr;
    size_t count = 0;
    if (!Attribute(data, size, kVoiceKind, kVoiceDown, &value, &count) || count == 0 || count > kMaxOpus || data[8] != kVersion) return false;
    out = {data[9], data[16], Rd16(data + 10), Rd32(data + 12), value, count};
    return true;
}

inline std::vector<uint8_t> EncodeControlUp(uint8_t op, uint64_t xuid, uint8_t self, const std::vector<uint64_t> &mutes, const uint8_t transaction[12]) {
    if (op < kSubscribe || op > kLeave || self > 3 || mutes.size() > kMaxSeats) return {};
    std::vector<uint8_t> v(12 + mutes.size() * 8, 0);
    v[0] = kVersion;
    v[1] = op;
    Be64(v.data() + 2, xuid);
    v[10] = self;
    v[11] = static_cast<uint8_t>(mutes.size());
    for (size_t i = 0; i < mutes.size(); ++i) Be64(v.data() + 12 + i * 8, mutes[i]);
    return Indication(kControlKind, kControlUp, v.data(), v.size(), transaction);
}

struct Seat {
    uint8_t seat = 0;
    uint64_t xuid = 0;
    uint8_t state = 0;
};
struct StateDown {
    uint8_t lobby = 0, bundle = 3;
    std::vector<Seat> seats;
};

inline bool DecodeStateDown(const uint8_t *data, size_t size, StateDown &out) {
    const uint8_t *v = nullptr;
    size_t count = 0;
    if (!Attribute(data, size, kControlKind, kStateDown, &v, &count) || count < 4 || v[0] != kVersion) return false;
    const size_t seats = v[3];
    if (v[1] > 1 || (v[2] != 3 && v[2] != 6) || seats > kMaxSeats || count != 4 + seats * 10) return false;
    StateDown s{v[1], v[2], {}};
    for (size_t i = 0; i < seats; ++i) {
        const uint8_t *at = v + 4 + i * 10;
        if (at[9] > 7) return false;
        s.seats.push_back({at[0], Rd64(at + 1), at[9]});
    }
    out = std::move(s);
    return true;
}

} // namespace fi::voice_wire
