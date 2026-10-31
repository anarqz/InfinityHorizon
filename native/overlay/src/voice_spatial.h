// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: proximity voice's spatial processing (NR-7; docs/launcher/PROXIMITY_VOICE.md §3, owner decisions of
// 2026-10-31). Header-only and free of Windows, so the private tests check it sample by sample. The mixer
// (voice_audio.cpp) owns one SeatProcessor and one Track per relay voice seat and one Scene (the listener).
//
// Per talker, per 5 ms block: both cars are extrapolated to "now" with their velocity and the poses' age; the vector
// between them is put in the listener's frame (its car's quaternion, else its heading from velocity, else the last
// heading); distance and direction are smoothed (critically damped, ~50 ms); the distance gives the gain (full within
// r0 = 10 m, inverse distance beyond, a raised-cosine fade to silence from 0.8 R to R = 150 m) and the Doppler delay;
// the direction gives an equal-power pan (pan = sin(azimuth), at most ±0.8). Doppler is a fractional delay line, read
// s·r(t)/c behind the write point (c = 343 m/s, s = 0.75) with 4-point Hermite interpolation: the pitch follows from
// the change of r(t), the buffer never drifts or underruns, and the read rate is held to 0.5-2. A teleport (the jump
// flag, or a move v·dt + 10 m does not explain) crossfades the old and the new read taps over 30 ms, with no pitch
// sweep; a change of kind (open ↔ spatial ↔ "far") crossfades over 200 ms.
//
// Edge cases (owner may change them): a talker with no car or a stale pose (> 1.5 s) holds its last position for 3 s,
// then plays centred at the edge-of-range gain (EdgeGain: heard, but far); a listener with no car hears everyone open
// (centred, unit gain, no Doppler). Open mode is the old mono path, both channels bit-identical to it.
//
// Time is the mixer's audio clock in seconds (samples mixed / 48000): poses are stamped with it when they arrive, so
// extrapolation is smooth even though mix frames are computed in bursts.
#pragma once

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <vector>

namespace fi::audio::spatial {

constexpr int kRate = 48000;
constexpr float kPi = 3.14159265358979f;

// The lobby's voice mode (UiVoiceOp.LobbyVoice's proposed 0/1/2). The mixer treats Off like Open: whether voice runs
// at all is decided elsewhere.
enum class Mode : uint8_t { Off = 0, Open = 1, Proximity = 2 };

struct Vec3 {
    float x = 0.0f, y = 0.0f, z = 0.0f;
};
constexpr Vec3 operator+(Vec3 a, Vec3 b) { return {a.x + b.x, a.y + b.y, a.z + b.z}; }
constexpr Vec3 operator-(Vec3 a, Vec3 b) { return {a.x - b.x, a.y - b.y, a.z - b.z}; }
constexpr Vec3 operator*(Vec3 a, float k) { return {a.x * k, a.y * k, a.z * k}; }
constexpr float Dot(Vec3 a, Vec3 b) { return a.x * b.x + a.y * b.y + a.z * b.z; }
constexpr Vec3 Cross(Vec3 a, Vec3 b) { return {a.y * b.z - a.z * b.y, a.z * b.x - a.x * b.z, a.x * b.y - a.y * b.x}; }
inline float Length(Vec3 a) { return std::sqrt(Dot(a, a)); }

// ---------------------------------------------------------------- axis convention: the ONE place to change it
// SAURON's RE of CarPredictionFull (decompile, 2026-10-31; live check pending): world Y up, left-handed, +X right,
// +Z forward (DirectXMath). The orientation is a unit quaternion (x, y, z, w), w >= 0, whose rotation rows are the
// car's right, up and forward (BasisFromQuaternion). The velocity heading uses right = up × forward, which equals the
// quaternion's right row for the same yaw. If the live left/right check (a car on the driver's right must give
// dot(d, right) > 0) finds the sides swapped, flip kRightSign; another up axis changes kWorldUp and kWorldForward.
constexpr Vec3 kWorldUp{0.0f, 1.0f, 0.0f};
constexpr Vec3 kWorldForward{0.0f, 0.0f, 1.0f}; // the heading before any is known
constexpr float kRightSign = 1.0f;              // +1: dot(talker - listener, right) > 0 is the listener's right ear

// One car as the relay saw it (PROXIMITY_VOICE.md §1, option C).
struct Pose {
    Vec3 position;                         // metres, world
    Vec3 velocity;                         // m/s, world
    float orientation[4] = {0, 0, 0, 1};   // x, y, z, w (OrientationFromWire); used when has_orientation
    bool has_orientation = false;
    bool has_car = false;                  // false: menus, loading, garage, a car swap
    bool jumped = false;                   // the relay saw a teleport (fast travel, reset, respawn, a new car)
    uint32_t age_ms = 0;                   // how old the sample was when its packet arrived
};

// The wire's 48-bit orientation (CarPredictionFull bits 192-239): x, y, z as s16 = round(clamp(±1) × 32767), w
// dropped (w >= 0 by construction).
inline void OrientationFromWire(int16_t sx, int16_t sy, int16_t sz, float q[4]) {
    q[0] = std::clamp(sx / 32767.0f, -1.0f, 1.0f);
    q[1] = std::clamp(sy / 32767.0f, -1.0f, 1.0f);
    q[2] = std::clamp(sz / 32767.0f, -1.0f, 1.0f);
    q[3] = std::sqrt(std::max(0.0f, 1.0f - q[0] * q[0] - q[1] * q[1] - q[2] * q[2]));
}

struct Basis {
    Vec3 right, up, forward;
};
// The rotation rows of a unit quaternion (DirectXMath, row vectors).
inline Basis BasisFromQuaternion(const float q[4]) {
    const float x = q[0], y = q[1], z = q[2], w = q[3];
    return {{1 - 2 * (y * y + z * z), 2 * (x * y + z * w), 2 * (x * z - y * w)},
            {2 * (x * y - z * w), 1 - 2 * (x * x + z * z), 2 * (y * z + x * w)},
            {2 * (x * z + y * w), 2 * (y * z - x * w), 1 - 2 * (x * x + y * y)}};
}

// ---------------------------------------------------------------- tuning (owner: listening test)

struct Params {
    float range_m = 150.0f;   // R: silent at and beyond (OD-164)
    float full_m = 10.0f;     // r0: full gain within (OD-164)
    float fade_from = 0.8f;   // the fade to silence starts at fade_from × R (OD-164)
    float doppler = 0.75f;    // s: 0 turns Doppler off; open question for the owner's listening test (0.5-1)
    float pan_limit = 0.8f;   // |pan| at most this: some crossfeed, so headphones never go one-eared
    bool rear_lowpass = false; // a gentle low-pass for talkers behind (front/back hint); off by default
};

constexpr float kSpeedOfSound = 343.0f;  // m/s
constexpr float kMaxDelayS = 0.5f;       // the delay line's reach: s·R/c must fit
constexpr float kMinRatio = 0.5f, kMaxRatio = 2.0f; // the read rate (pitch ratio) is held to this
constexpr float kSmoothS = 0.05f;        // critically damped smoothing of distance and direction
constexpr float kMaxRadialAccel = 500.0f; // m/s²: the slew limit of dr/dt (a 30 m/s pass-by 2 m off peaks ~450)
constexpr float kJumpSlackM = 10.0f;     // a move v·dt + this does not explain is a teleport
constexpr double kStaleS = 1.5;          // a pose older than this is stale
constexpr double kHoldS = 3.0;           // a stale or carless talker holds its last position this long
constexpr float kHeadingMinSpeed = 3.0f; // m/s: below, the velocity heading keeps the last one
constexpr float kJumpFadeS = 0.03f, kKindFadeS = 0.2f;

// Out-of-range tuning made safe (a listening-test build may set anything).
inline Params Sanitized(Params p) {
    auto finite = [](float v, float fallback) { return std::isfinite(v) ? v : fallback; };
    p.range_m = std::clamp(finite(p.range_m, 150.0f), 20.0f, 1000.0f);
    p.full_m = std::clamp(finite(p.full_m, 10.0f), 1.0f, p.range_m * 0.5f);
    p.fade_from = std::clamp(finite(p.fade_from, 0.8f), 0.5f, 0.99f);
    const float most = (kMaxDelayS - 0.01f) * kSpeedOfSound / p.range_m; // s·R/c within the line
    p.doppler = std::clamp(finite(p.doppler, 0.75f), 0.0f, std::min(1.0f, most));
    p.pan_limit = std::clamp(finite(p.pan_limit, 0.8f), 0.0f, 1.0f);
    return p;
}

// Amplitude at distance r: 1 within r0, r0/r beyond (-6 dB per doubling), times a raised-cosine fade from
// fade_from × R to silence at R.
inline float DistanceGain(float r, const Params &p) {
    if (std::isnan(r)) return 0.0f;
    if (r <= p.full_m) return 1.0f;
    if (r >= p.range_m) return 0.0f;
    float g = p.full_m / r;
    const float from = p.fade_from * p.range_m;
    if (r > from) g *= 0.5f * (1.0f + std::cos(kPi * (r - from) / (p.range_m - from)));
    return g;
}
// "Heard, but far": the gain where the fade starts (10/120 = -21.6 dB). A talker with no car or a stale pose plays
// centred at this level after its 3 s hold (owner may prefer silence).
inline float EdgeGain(const Params &p) { return DistanceGain(p.fade_from * p.range_m, p); }

// Equal-power stereo gains of a pan in [-1, 1], scaled so the centre is 1.0 per channel: the open mode's level (a mono
// source voice reaches a stereo output at 1.0 per channel in XAudio2/FAudio), so a centred talker is as loud in
// either mode and the power L² + R² stays 2 wherever the talker is.
inline void PanGains(float pan, float &left, float &right) {
    const float a = (std::clamp(pan, -1.0f, 1.0f) + 1.0f) * (kPi / 4.0f);
    left = 1.41421356f * std::cos(a);
    right = 1.41421356f * std::sin(a);
}

// ---------------------------------------------------------------- one car over time

struct Track {
    enum class State : uint8_t { Gone, Live, Held };
    bool have_car = false; // a pose with a car arrived (since the track was made)
    Pose car;              // the newest pose with a car
    double car_at = 0;     // when that sample was taken, audio clock (arrival - age)
    double lost_at = -1;   // a pose without a car arrived at this time, after `car`; -1: none
    uint32_t jumps = 0;    // teleports seen: the jump flag, a move v·dt + 10 m does not explain, or a car again

    void Update(const Pose &p, double arrived) {
        const double at = arrived - p.age_ms / 1000.0;
        if (!p.has_car) {
            if (have_car && lost_at < 0) lost_at = arrived;
            return;
        }
        if (have_car) {
            if (at < car_at) return; // reordered: older than what is here
            const float explained = std::max(Length(car.velocity), Length(p.velocity)) * static_cast<float>(at - car_at) + kJumpSlackM;
            if (p.jumped || lost_at >= 0 || Length(p.position - car.position) > explained) jumps++;
        }
        car = p;
        car_at = at;
        have_car = true;
        lost_at = -1;
    }
    double StaleFrom() const { return lost_at >= 0 ? lost_at : car_at + kStaleS; }
    // Where the car is at t: Live (extrapolated, at most kStaleS), Held (frozen where it went stale, for kHoldS), Gone.
    State At(double t, Vec3 &pos) const {
        if (!have_car) return State::Gone;
        const double stale = StaleFrom();
        const double until = std::clamp(std::min(t, stale) - car_at, 0.0, kStaleS);
        pos = car.position + car.velocity * static_cast<float>(until);
        if (t < stale) return State::Live;
        return t < stale + kHoldS ? State::Held : State::Gone;
    }
};

// The listener's ears: from its car's quaternion when the pose has one, else the horizontal velocity above 3 m/s,
// else the last heading (up = kWorldUp).
struct Heading {
    Vec3 right = Cross(kWorldUp, kWorldForward), forward = kWorldForward;
    void Update(const Pose &self) {
        if (self.has_orientation) {
            const Basis b = BasisFromQuaternion(self.orientation);
            right = b.right;
            forward = b.forward;
            return;
        }
        const Vec3 h = self.velocity - kWorldUp * Dot(self.velocity, kWorldUp);
        const float speed = Length(h);
        if (speed > kHeadingMinSpeed) {
            forward = h * (1.0f / speed);
            right = Cross(kWorldUp, forward);
        }
    }
};

// What every seat shares for a mix frame.
struct Scene {
    Params params;
    bool proximity = false; // the lobby's voice mode is Proximity
    Track listener;
    Heading heading;
    void SetListener(const Pose &self, double arrived) {
        listener.Update(self, arrived);
        if (self.has_car) heading.Update(self);
    }
    // The listener's position at t; false: hear everyone open (no car, or lost it; a stale pose holds 3 s first).
    bool ListenerAt(double t, Vec3 &pos) const {
        const Track::State s = listener.At(t, pos);
        return s == Track::State::Live || (s == Track::State::Held && listener.lost_at < 0);
    }
};

// ---------------------------------------------------------------- one talker's processing

class SeatProcessor {
public:
    static constexpr int kLine = 32768, kMask = kLine - 1; // 0.68 s: the 0.5 s reach + Hermite's taps
    static constexpr int kBlock = 240;                      // 5 ms: poses, smoothing and the delay's slope per block
    static constexpr float kMinDelay = 2.0f;                // Hermite reads two samples ahead of its point
    static constexpr float kMaxDelay = kMaxDelayS * kRate;
    enum class Kind : uint8_t { Open, Spatial, Far };

    SeatProcessor() : line_(kLine, 0.0f) {}

    // Silence at once (a silenced seat): the line is cleared, nothing more plays.
    void Reset() {
        std::fill(line_.begin(), line_.end(), 0.0f);
        tail_ = fade_left_ = 0;
        cur_ = Tap{};
        snap_ = true;
        lp_ = 0.0f;
    }
    // Still producing output (input this frame, or the delay line's tail: 0.68 s after the last input).
    bool Busy() const { return tail_ > 0; }

    // Adds n samples (a multiple of kBlock) to `stereo` (interleaved L, R) starting at audio time t0. in[0..filled) is
    // this talker's decoded voice (seat volume applied), silence after. Open with no fade adds exactly in[k] to both.
    void Process(const float *in, int filled, int n, float *stereo, double t0, const Scene &scene, const Track &talker) {
        if (filled > 0) tail_ = kLine;
        else if (tail_ <= 0) { snap_ = true; return; } // idle: the line holds only zeros now
        const float dt = static_cast<float>(kBlock) / kRate;
        for (int b = 0; b + kBlock <= n; b += kBlock) {
            const Target g = Evaluate(t0 + static_cast<double>(b + kBlock) / kRate, scene, talker);
            const bool jump = talker.jumps != seen_talker_ || scene.listener.jumps != seen_listener_;
            seen_talker_ = talker.jumps;
            seen_listener_ = scene.listener.jumps;
            if (snap_) { // a spurt from an idle seat: nothing plays yet, so it starts where it should (a far talker later)
                Snap(g, scene.params);
                fade_left_ = 0;
                snap_ = false;
            } else if (g.kind != cur_.kind) {
                StartFade(kKindFadeS);
                Snap(g, scene.params);
            } else if (jump && g.kind == Kind::Spatial) {
                StartFade(kJumpFadeS);
                Snap(g, scene.params);
            }
            Tap next = cur_;
            if (g.kind == Kind::Spatial) {
                const Params &p = scene.params;
                if (g.has_dir) { lat_t_ = g.lat; front_t_ = g.front; }
                r_.Step(g.r, dt, kMaxRadialAccel);
                lat_.Step(lat_t_, dt, 1e9f);
                front_.Step(front_t_, dt, 1e9f);
                const float want = std::min(kMinDelay + p.doppler * std::min(std::max(r_.x, 0.0f), p.range_m) / kSpeedOfSound * kRate, kMaxDelay);
                // read rate = 1 - d(delay)/d(sample), held to [kMinRatio, kMaxRatio]
                next.delay = cur_.delay + std::clamp(want - cur_.delay, -(kMaxRatio - 1.0f) * kBlock, (1.0f - kMinRatio) * kBlock);
                Gains(p, DistanceGain(std::max(r_.x, 0.0f), p), next);
                ratio_ = 1.0f - (next.delay - cur_.delay) / kBlock;
            } else {
                next = FixedTap(g.kind, scene.params);
                ratio_ = 1.0f;
            }
            Render(in, filled, b, stereo, cur_, next, scene.params.rear_lowpass && g.kind == Kind::Spatial);
            cur_ = next;
        }
        if (filled <= 0) tail_ -= n;
    }

    // For the tests and the bench.
    Kind CurrentKind() const { return cur_.kind; }
    float DelaySamples() const { return cur_.delay; }
    float LeftGain() const { return cur_.left; }
    float RightGain() const { return cur_.right; }
    float Ratio() const { return ratio_; }   // the last block's read rate (the Doppler pitch ratio)
    float Distance() const { return r_.x; }  // smoothed
    bool Fading() const { return fade_left_ > 0; }
    // The louder channel's gain now (1 open; 0 beyond range): with SeatTalking, "heard now" for the talk indicator.
    float AudibleGain() const { return std::max(cur_.left, cur_.right); }

private:
    struct Tap {
        Kind kind = Kind::Open;
        float delay = 0.0f, left = 1.0f, right = 1.0f; // delay 0: the input itself (Open, Far)
    };
    struct Target {
        Kind kind = Kind::Open;
        float r = 0.0f, lat = 0.0f, front = 1.0f;
        bool has_dir = false;
    };
    // Critically damped (ω = 2 / kSmoothS), semi-implicit, its acceleration limited (the slew of dr/dt).
    struct Damped {
        float x = 0.0f, v = 0.0f;
        void Snap(float to) { x = to; v = 0.0f; }
        void Step(float target, float dt, float max_accel) {
            constexpr float w = 2.0f / kSmoothS;
            const float a = std::clamp(w * w * (target - x) - 2.0f * w * v, -max_accel, max_accel);
            v += a * dt;
            x += v * dt;
        }
    };

    Target Evaluate(double t, const Scene &scene, const Track &talker) const {
        Target g;
        Vec3 me, him;
        if (!scene.proximity || !scene.ListenerAt(t, me)) return g; // Open
        if (talker.At(t, him) == Track::State::Gone) { g.kind = Kind::Far; return g; }
        g.kind = Kind::Spatial;
        const Vec3 d = him - me;
        g.r = Length(d);
        const float lat = kRightSign * Dot(d, scene.heading.right), front = Dot(d, scene.heading.forward);
        const float h = std::sqrt(lat * lat + front * front);
        if (h > 0.5f) { g.lat = lat / h; g.front = front / h; g.has_dir = true; } // closer: keep the last direction
        return g;
    }
    static Tap FixedTap(Kind kind, const Params &p) {
        Tap t;
        t.kind = kind;
        if (kind == Kind::Far) t.left = t.right = EdgeGain(p);
        return t;
    }
    void Gains(const Params &p, float gain, Tap &t) const {
        const float h = std::sqrt(lat_.x * lat_.x + front_.x * front_.x);
        const float pan = h > 1e-3f ? std::clamp(lat_.x / h, -p.pan_limit, p.pan_limit) : 0.0f; // sin(azimuth)
        PanGains(pan, t.left, t.right);
        t.left *= gain;
        t.right *= gain;
    }
    void Snap(const Target &g, const Params &p) {
        if (g.kind != Kind::Spatial) { cur_ = FixedTap(g.kind, p); return; }
        if (g.has_dir) { lat_t_ = g.lat; front_t_ = g.front; }
        r_.Snap(g.r);
        lat_.Snap(lat_t_);
        front_.Snap(front_t_);
        cur_.kind = Kind::Spatial;
        cur_.delay = std::min(kMinDelay + p.doppler * std::min(g.r, p.range_m) / kSpeedOfSound * kRate, kMaxDelay);
        Gains(p, DistanceGain(g.r, p), cur_);
    }
    void StartFade(float seconds) {
        old_ = cur_; // frozen: a constant delay reads at rate 1, so the fading tap never sweeps
        fade_len_ = fade_left_ = static_cast<int>(seconds * kRate);
    }
    // 4-point, 3rd-order Hermite at `delay` samples behind the newest written sample (delay >= 2).
    float Read(float delay) const {
        const int di = static_cast<int>(delay);
        const float fd = delay - static_cast<float>(di);
        uint32_t i = w_ - static_cast<uint32_t>(di);
        float f = 0.0f;
        if (fd > 0.0f) { i -= 1; f = 1.0f - fd; }
        const float xm1 = line_[(i - 1) & kMask], x0 = line_[i & kMask], x1 = line_[(i + 1) & kMask], x2 = line_[(i + 2) & kMask];
        const float c1 = 0.5f * (x1 - xm1), c2 = xm1 - 2.5f * x0 + 2.0f * x1 - 0.5f * x2, c3 = 0.5f * (x2 - xm1) + 1.5f * (x0 - x1);
        return ((c3 * f + c2) * f + c1) * f + x0;
    }
    void Render(const float *in, int filled, int at, float *stereo, const Tap &a, const Tap &b, bool rear) {
        const bool exact = a.kind == Kind::Open && b.kind == Kind::Open;
        const float step = 1.0f / kBlock;
        const float rear_amount = rear ? 0.7f * std::clamp(-front_.x, 0.0f, 1.0f) : 0.0f;
        for (int i = 0; i < kBlock; i++) {
            const int k = at + i;
            const float x = k < filled ? in[k] : 0.0f;
            line_[w_ & kMask] = x;
            float yl, yr;
            if (exact) {
                yl = yr = x;
            } else {
                const float f = (i + 1) * step;
                float s = b.kind == Kind::Spatial ? Read(a.delay + (b.delay - a.delay) * f) : x;
                if (rear_amount > 0.0f) { lp_ += 0.28f * (s - lp_); s += rear_amount * (lp_ - s); } // ~2.5 kHz one-pole
                yl = s * (a.left + (b.left - a.left) * f);
                yr = s * (a.right + (b.right - a.right) * f);
            }
            if (fade_left_ > 0) {
                const float o = old_.kind == Kind::Spatial ? Read(old_.delay) : x;
                const float w = 0.5f - 0.5f * std::cos(kPi * static_cast<float>(fade_len_ - fade_left_ + 1) / static_cast<float>(fade_len_));
                yl = o * old_.left * (1.0f - w) + yl * w;
                yr = o * old_.right * (1.0f - w) + yr * w;
                fade_left_--;
            }
            stereo[2 * k] += yl;
            stereo[2 * k + 1] += yr;
            w_++;
        }
    }

    std::vector<float> line_;
    uint32_t w_ = 0;      // index of the newest written sample (wraps; masked)
    int tail_ = 0;        // samples still to run after the last input (the line empties into the mix)
    bool snap_ = true;    // idle: the next block starts at its target, no smoothing, no fade
    Tap cur_, old_;
    int fade_left_ = 0, fade_len_ = 1;
    Damped r_, lat_, front_;
    float lat_t_ = 0.0f, front_t_ = 1.0f;
    float ratio_ = 1.0f, lp_ = 0.0f;
    uint32_t seen_talker_ = 0, seen_listener_ = 0;
};

} // namespace fi::audio::spatial
