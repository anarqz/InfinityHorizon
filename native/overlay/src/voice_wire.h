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
// CR-L46 (MATCHMAKING_API.md §5c "Proximity"): optional attributes after the message's own, found anywhere, skipped by their
// padded length; a malformed optional attribute is ignored and the datagram kept; one running past the datagram drops it:
//   0xC0F8 pose, after 0xC0F4 (proximity only), 48 bytes: {ver, flags (bit 0 talker car, 1 listener car, 2 same side, 3
//          reserved, 4-5 talker jumps, 6-7 listener jumps), talker {age u16 ms, x y z f32, vx vy vz s16}, listener {age,
//          x y z, vx vy vz}, listener orientation qx qy qz s16 (w dropped)}
//   0xC0F9 mode, after 0xC0F6 (every state down from a new relay), 4 bytes: {ver, mode (0 off, 1 open, 2 proximity), range
//          u16 (R in metres in proximity, else 0)}
#pragma once

#include <cmath>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <vector>

namespace fi::voice_wire {

constexpr uint16_t kVoiceKind = 0x3E93, kControlKind = 0x3E94;
constexpr uint16_t kVoiceUp = 0xC0F3, kVoiceDown = 0xC0F4, kControlUp = 0xC0F5, kStateDown = 0xC0F6;
constexpr uint16_t kPose = 0xC0F8, kMode = 0xC0F9;
constexpr uint8_t kVersion = 1;
constexpr size_t kMaxDatagram = 548, kMaxOpus = 400, kMaxSeats = 12, kPoseSize = 48, kModeSize = 4;
constexpr uint32_t kMagic = 0x2112A442;
enum : uint8_t { kModeOff = 0, kModeOpen = 1, kModeProximity = 2 };
enum : uint8_t { kPoseTalkerCar = 1, kPoseListenerCar = 2, kPoseSameSide = 4 };
constexpr uint16_t kMinRange = 20; // a proximity range below this ignores 0xC0F9

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

// Indications with several attributes in this order, each padded to 4 bytes (the message's attribute first); empty past 548.
struct Attr {
    uint16_t tag;
    const uint8_t *value;
    size_t size;
};
inline std::vector<uint8_t> Indication(uint16_t kind, const Attr *attrs, size_t n, const uint8_t transaction[12]) {
    size_t length = 20;
    for (size_t i = 0; i < n; ++i) length += 4 + (attrs[i].size + 3) / 4 * 4;
    if (length > kMaxDatagram) return {};
    std::vector<uint8_t> out(length, 0);
    Be16(out.data(), kind);
    Be16(out.data() + 2, static_cast<uint16_t>(length - 20));
    Be32(out.data() + 4, kMagic);
    std::memcpy(out.data() + 8, transaction, 12);
    size_t at = 20;
    for (size_t i = 0; i < n; ++i) {
        Be16(out.data() + at, attrs[i].tag);
        Be16(out.data() + at + 2, static_cast<uint16_t>(attrs[i].size));
        if (attrs[i].size) std::memcpy(out.data() + at + 4, attrs[i].value, attrs[i].size);
        at += 4 + (attrs[i].size + 3) / 4 * 4;
    }
    return out;
}

// The attribute's value when `data` is a well-formed indication of `kind` carrying `attribute`: the whole datagram walks
// (an attribute running past its end fails the shape, wherever it is) and the attribute is found anywhere in it.
inline bool Attribute(const uint8_t *data, size_t size, uint16_t kind, uint16_t attribute, const uint8_t **value, size_t *value_size) {
    if (size < 20 || size > kMaxDatagram || size % 4 || (data[0] & 0xC0) || Rd32(data + 4) != kMagic || Rd16(data + 2) + 20u != size || Rd16(data) != kind)
        return false;
    bool found = false;
    for (size_t at = 20; at + 4 <= size;) {
        const uint16_t tag = Rd16(data + at), count = Rd16(data + at + 2);
        if (count > size - at - 4) return false;
        if (tag == attribute && !found) {
            *value = data + at + 4;
            *value_size = count;
            found = true;
        }
        at += 4 + (count + 3u) / 4 * 4;
    }
    return found;
}

// One side of a pose: a car the relay knows, its sample's age (ms at the relay's send), position (m), raw velocity and the
// low 2 bits of that player's jump counter. `car` false: every field zero but `jumps`.
struct PoseSide {
    bool car = false;
    uint16_t age = 0;
    float pos[3] = {0, 0, 0};
    int16_t vel[3] = {0, 0, 0};
    uint8_t jumps = 0;
};
struct Pose {
    bool present = false;  // a usable 0xC0F8 came with the voice
    uint8_t flags = 0;     // as received (bit 3 reserved, ignored)
    PoseSide talker, listener;
    int16_t orient[3] = {0, 0, 0}; // the listener's quaternion x, y, z, raw (zero without its car)
};

inline float RdF32(const uint8_t *p) {
    const uint32_t bits = Rd32(p);
    float f;
    std::memcpy(&f, &bits, 4);
    return f;
}
inline void BeF32(uint8_t *p, float f) {
    uint32_t bits;
    std::memcpy(&bits, &f, 4);
    Be32(p, bits);
}

inline PoseSide DecodePoseSide(const uint8_t *at, bool car, uint8_t jumps) {
    PoseSide s;
    s.jumps = jumps;
    if (!car) return s;
    for (int i = 0; i < 3; ++i) s.pos[i] = RdF32(at + 2 + 4 * i);
    for (int i = 0; i < 3; ++i)
        if (!std::isfinite(s.pos[i])) return PoseSide{false, 0, {0, 0, 0}, {0, 0, 0}, jumps}; // a non-finite f32: no car
    s.car = true;
    s.age = Rd16(at);
    for (int i = 0; i < 3; ++i) s.vel[i] = static_cast<int16_t>(Rd16(at + 14 + 2 * i));
    return s;
}

// The pose of a voice down (ignored, not present, unless ver 1 and exactly 48 bytes).
inline Pose DecodePose(const uint8_t *v, size_t size) {
    Pose p;
    if (size != kPoseSize || v[0] != kVersion) return p;
    p.present = true;
    p.flags = v[1];
    p.talker = DecodePoseSide(v + 2, (v[1] & kPoseTalkerCar) != 0, static_cast<uint8_t>(v[1] >> 4 & 3));
    p.listener = DecodePoseSide(v + 22, (v[1] & kPoseListenerCar) != 0, static_cast<uint8_t>(v[1] >> 6 & 3));
    if (p.listener.car)
        for (int i = 0; i < 3; ++i) p.orient[i] = static_cast<int16_t>(Rd16(v + 42 + 2 * i));
    return p;
}

inline void EncodePoseSide(uint8_t *at, const PoseSide &s) {
    if (!s.car) return;
    Be16(at, s.age);
    for (int i = 0; i < 3; ++i) BeF32(at + 2 + 4 * i, s.pos[i]);
    for (int i = 0; i < 3; ++i) Be16(at + 14 + 2 * i, static_cast<uint16_t>(s.vel[i]));
}

// The pose's 48 bytes (flags from the sides: car bits, same side, jump counters).
inline void EncodePose(const Pose &p, bool same_side, uint8_t out[kPoseSize]) {
    std::memset(out, 0, kPoseSize);
    out[0] = kVersion;
    out[1] = static_cast<uint8_t>((p.talker.car ? kPoseTalkerCar : 0) | (p.listener.car ? kPoseListenerCar : 0) | (same_side ? kPoseSameSide : 0)
                                  | (p.talker.jumps & 3) << 4 | (p.listener.jumps & 3) << 6);
    EncodePoseSide(out + 2, p.talker);
    EncodePoseSide(out + 22, p.listener);
    if (p.listener.car)
        for (int i = 0; i < 3; ++i) Be16(out + 42 + 2 * i, static_cast<uint16_t>(p.orient[i]));
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
    Pose pose; // proximity: the cars at the relay's send (not present: play as open)
};

inline bool DecodeVoiceDown(const uint8_t *data, size_t size, VoiceDown &out) {
    const uint8_t *value = nullptr;
    size_t count = 0;
    if (!Attribute(data, size, kVoiceKind, kVoiceDown, &value, &count) || count == 0 || count > kMaxOpus || data[8] != kVersion) return false;
    out = {data[9], data[16], Rd16(data + 10), Rd32(data + 12), value, count, {}};
    const uint8_t *pose = nullptr;
    size_t pose_size = 0;
    if (Attribute(data, size, kVoiceKind, kPose, &pose, &pose_size)) out.pose = DecodePose(pose, pose_size); // a bad pose never drops the voice
    return true;
}

// Voice down as the relay writes it (tests; the golden vectors): `random` is the transaction id's last 3 bytes; a pose
// (when given) follows the Opus attribute.
inline std::vector<uint8_t> EncodeVoiceDown(uint8_t flags, uint16_t seq, uint32_t ts, uint8_t seat, const uint8_t *opus, size_t size, const uint8_t random[3],
                                            const Pose *pose = nullptr, bool same_side = true) {
    if (size == 0 || size > kMaxOpus) return {};
    uint8_t transaction[12] = {kVersion, flags};
    Be16(transaction + 2, seq);
    Be32(transaction + 4, ts);
    transaction[8] = seat;
    std::memcpy(transaction + 9, random, 3);
    uint8_t value[kPoseSize];
    if (pose) EncodePose(*pose, same_side, value);
    const Attr attrs[2] = {{kVoiceDown, opus, size}, {kPose, value, kPoseSize}};
    return Indication(kVoiceKind, attrs, pose ? 2 : 1, transaction);
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
    // What to play (normative): 0xC0F9's mode when present and consistent, else the mode from `lobby` (0 off, 1 open).
    uint8_t mode = kModeOff;
    uint16_t range = 0;        // R in metres in proximity (from the relay; never hard-coded), else 0
    bool mode_present = false; // a usable 0xC0F9 came with it
};

// 0xC0F9's receiver rules: ver 1, 4 bytes, mode <= 2, range >= 20 in proximity and 0 otherwise, (lobby = 0) == (mode = 0).
inline bool ModeConsistent(uint8_t lobby, uint8_t mode, uint16_t range) {
    return mode <= kModeProximity && (mode == kModeProximity ? range >= kMinRange : range == 0) && ((lobby == 0) == (mode == kModeOff));
}

inline bool DecodeStateDown(const uint8_t *data, size_t size, StateDown &out) {
    const uint8_t *v = nullptr;
    size_t count = 0;
    if (!Attribute(data, size, kControlKind, kStateDown, &v, &count) || count < 4 || v[0] != kVersion) return false;
    const size_t seats = v[3];
    if (v[1] > 1 || (v[2] != 3 && v[2] != 6) || seats > kMaxSeats || count != 4 + seats * 10) return false;
    StateDown s;
    s.lobby = v[1];
    s.bundle = v[2];
    for (size_t i = 0; i < seats; ++i) {
        const uint8_t *at = v + 4 + i * 10;
        if (at[9] > 7) return false;
        s.seats.push_back({at[0], Rd64(at + 1), at[9]});
    }
    s.mode = s.lobby == 1 ? kModeOpen : kModeOff;
    const uint8_t *m = nullptr;
    size_t m_size = 0;
    if (Attribute(data, size, kControlKind, kMode, &m, &m_size) && m_size == kModeSize && m[0] == kVersion && ModeConsistent(s.lobby, m[1], Rd16(m + 2))) {
        s.mode = m[1];
        s.range = Rd16(m + 2);
        s.mode_present = true;
    }
    out = std::move(s);
    return true;
}

// State down as a new relay writes it (tests; the golden vectors): 0xC0F9 after 0xC0F6 unless `with_mode` is false (a
// pre-CR-L46 relay). Empty for values the decoder would refuse.
inline std::vector<uint8_t> EncodeStateDown(const StateDown &s, bool with_mode, const uint8_t transaction[12]) {
    if (s.lobby > 1 || (s.bundle != 3 && s.bundle != 6) || s.seats.size() > kMaxSeats || (with_mode && !ModeConsistent(s.lobby, s.mode, s.range))) return {};
    std::vector<uint8_t> v(4 + s.seats.size() * 10, 0);
    v[0] = kVersion;
    v[1] = s.lobby;
    v[2] = s.bundle;
    v[3] = static_cast<uint8_t>(s.seats.size());
    for (size_t i = 0; i < s.seats.size(); ++i) {
        uint8_t *at = v.data() + 4 + i * 10;
        if (s.seats[i].state > 7) return {};
        at[0] = s.seats[i].seat;
        Be64(at + 1, s.seats[i].xuid);
        at[9] = s.seats[i].state;
    }
    uint8_t mode[kModeSize] = {kVersion, s.mode};
    Be16(mode + 2, s.range);
    const Attr attrs[2] = {{kStateDown, v.data(), v.size()}, {kMode, mode, kModeSize}};
    return Indication(kControlKind, attrs, with_mode ? 2 : 1, transaction);
}

} // namespace fi::voice_wire
