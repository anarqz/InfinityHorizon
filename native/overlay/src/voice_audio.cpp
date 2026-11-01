// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: voice chat's audio engine (OD-154, VC-3; VOICE_CHAT.md §3.2, §12.4). Two engine threads, never
// the game's: capture (WASAPI shared, event-driven; the player's chosen microphone or else the default communications
// one, never a monitor or loopback input: voice_audio_devices.h; none: listen-only; §15 levels: voice_volume.h) → 48 kHz mono → echo
// cancellation (WebRTC AEC3 + its 100 Hz high-pass, against the mix about to play: voice_echo.h) → AGC → RNNoise (noise suppression; its voice probability gates sending, 300 ms hangover) → Opus 20 ms
// frames (SILK wideband 6 kbit/s, CVBR, complexity 10, no FEC/DTX) → repacketized 3 (or 6) per packet → SendFn; and
// playback: per seat an adaptive 1-2 packet jitter buffer → Opus decoder with deep PLC for gaps → stereo mix (open:
// every talker centred, both channels the old mono mix; proximity: voice_spatial.h places each by its car) → one
// WASAPI shared render stream on the output (~60 ms queued; resampled and mapped to the output's own format). The decoder
// starts at complexity 7 (NoLACE) on every platform and steps
// down only on measured decode cost (voice_audio_complexity.h): no platform-based quality choice, so a Deck player
// gets what a Windows player gets. Releasing transmit (push-to-talk, mute) ends the spurt with a short packet that
// carries the end flag. Every codec failure logs and turns voice off; a missing, unplugged or changed device is retried
// or reopened every 2-3 s (backing off to 16 s when the device keeps failing). Nothing here touches the network.
//
// The game is never held by a device (2026-10-31: an output reopen under Proton never returned and the game froze).
// Every device open, poll, reopen and close (capture, playback, loopback) runs on the session's own device workers;
// nothing the game's threads call (Receive, SetPose, SeatTalking, SeatHeard, Running, ListenOnly, Sending) waits for
// them: those take the session lock only with a try (Start and Stop hold it exclusively just to swap a pointer, never
// across a device call or a thread wait). A watchdog abandons a worker whose device call has not returned for 8 s (a
// fresh worker retries, with backoff; the abandoned one keeps its own reference to the session, so nothing is freed
// under it, and it touches nothing shared when it comes back). Stop waits 1.5 s at most, then leaves stuck workers
// behind. Playback is WASAPI, not XAudio2: the game's own XAudio2 runs on the same xaudio2_9/FAudio library in this
// process, and a voice mastering voice being torn down and rebuilt on a device change shares that library's state
// with the game (inference for the freeze: the game's main thread waited on a lock while the voice output reopened).
//
// Wine/Proton audit (Windows/Deck parity). Every API the engine calls, and why it behaves the same on both. "Known":
// Wine's implementation (mmdevapi + winepulse, FAudio-backed xaudio2_9) has it, from its sources and Proton's use by
// games; "inference": reasoned, not yet run here (SAURON's Deck run checks it: voice_audio_test + a device session).
//
// | API / behaviour                                | Windows | Wine/Proton                                   | Engine's choice                                        |
// | ---------------------------------------------- | ------- | --------------------------------------------- | ------------------------------------------------------ |
// | CoInitializeEx(MTA), CoCreateInstance(MMDeviceEnumerator) | yes | known (mmdevapi)                     | used as is                                             |
// | GetDefaultAudioEndpoint(eCapture, eCommunications) | yes | known: accepted; every role maps to the one default (inference) | used; same device as eConsole there   |
// | IMMDevice::GetId, Activate(IAudioClient)       | yes     | known                                         | used                                                   |
// | EnumAudioEndpoints(ACTIVE), GetDevice, OpenPropertyStore(FriendlyName), IMMEndpoint::GetDataFlow | yes | known: winepulse lists the output's monitor as a capture endpoint named "Monitor of ..." | monitors/loopbacks rejected by name (localized too; "<words> <an output's name>"), id and data flow; none left: listen-only |
// | EnumAudioEndpoints: the sound server's default as its own entry | no | known: a "PulseAudio"/PipeWire default stand-in; with no microphone it captures the output's monitor | never opened nor listed (voice_audio_devices.h IsDefaultAlias) |
// | GetMixFormat → WAVEFORMATEXTENSIBLE float32    | usual   | known: winepulse reports extensible float32 at the PulseAudio/PipeWire rate (44.1 or 48 kHz), N channels | any PCM8/16/24/32 or float32/64, any rate/channels, converted by voice_audio_dsp.h; the delivered rate is watched |
// | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM / SRC_DEFAULT_QUALITY | Win10+ | newer Wine only; quality differs (inference) | **not used**: our own downmix + windowed-sinc resampler on both |
// | Shared mode + AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 100 ms buffer | yes | known; Wine rounds the period to its own (~10 ms) | packet sizes are not assumed: every wake drains all packets (copied, released, then processed); 500 ms wait timeout as a fallback |
// | AUDCLNT_E_DEVICE_INVALIDATED on unplug         | yes     | not guaranteed: PulseAudio moves the stream (inference) | also: no data for 2 s = reopen; default device id polled every 3 s (capture and render) |
// | A device call that blocks (a sink switching a2dp ↔ alsa) | rare | known (2026-10-31): mmdevapi/winepulse may block for seconds or more | on device workers only; a watchdog abandons one stuck 8 s, a fresh one retries |
// | IMMNotificationClient                          | yes     | partial (inference)                           | **not used**: polling works on both                    |
// | MMCSS (AvSetMmThreadCharacteristics, avrt.dll) | yes     | stub / may be absent                          | **not used** (no avrt import); plain threads          |
// | FTZ/DAZ via _mm_setcsr on each engine thread   | yes     | known: MXCSR is per thread CPU state, Wine keeps it | set on capture, playback and test-seam entry       |
// | WASAPI shared render, EVENTCALLBACK, the output's mix format | yes | known (winepulse)               | 100 ms buffer kept ~60 ms full; 20 ms wait timeout so a missed event never starves it; our resampler + channel map (L→ch0, R→ch1, mono = (L+R)/2) |
// | IMMDeviceEnumerator::GetDevice(the player's output id, §15) | yes | known                              | refused: the default output, logged once, no reopen loop |
// | XAudio2 for voice                              | yes     | the game's own XAudio2 shares the library     | **not used** (only the one-shot toast chime keeps it)  |
// | Processor affinity / priority, GetActiveProcessorCount, IsProcessorFeaturePresent, wine_get_version | yes | yes | **not used**: no platform input anywhere |
// | QueryPerformanceCounter, GetTickCount64, CreateThread, events | yes | known                                  | used (decode-cost timing, clocks, threads)             |
// | libopus RTCD (cpuid: SSE4.1/AVX2)              | yes     | known: cpuid is the real CPU (Deck: Zen 2, AVX2) | same code paths on both CPUs with AVX2              |
// | WebRTC AEC3 cpu_features (cpuid + xgetbv: AVX2) | yes    | known: both are CPU instructions, no OS call  | same AEC3 paths on both CPUs with AVX2; no audio API   |
// | AUDCLNT_STREAMFLAGS_LOOPBACK on the output in use (echo reference only) | yes | inference: winepulse's monitor; older Wine may refuse | polled every 10 ms (no event); refused: the mix is the reference |
#include "voice_audio.h"
#include "voice_audio_complexity.h"
#include "voice_audio_devices.h"
#include "voice_audio_dsp.h"
#include "voice_audio_testing.h"
#include "voice_echo.h"
#include "voice_spatial.h"

#include "overlay.h"

#include <mmreg.h>
#include <audioclient.h>
#include <mmdeviceapi.h>
#include <xmmintrin.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <cmath>
#include <cstdio>
#include <deque>
#include <memory>
#include <mutex>
#include <new>
#include <shared_mutex>
#include <string>
#include <vector>

#include "opus.h"
#include "rnnoise.h"

namespace fi::audio {
namespace {

constexpr int kRate = 48000, kSub = 480, kFrame = 960, kMaxBundle = 6, kSeats = 16;
constexpr int kHangoverSubs = 30; // 300 ms of 10 ms blocks
constexpr float kVoiceThreshold = 0.5f;
constexpr int64_t kTalkingMs = 300;
constexpr int kMaxConcealFrames = 6;   // a talker gone quiet without an end flag: 120 ms of PLC, then silence
constexpr size_t kMaxBuffered = 4;     // packets per seat; older ones are dropped (latency bound)
constexpr size_t kMaxInbox = 256;
constexpr int kQueuedMs = 60;          // the render stream is kept this full: the ~60 ms target
constexpr DWORD kRetryMs = 2000;

std::atomic<int64_t> g_fake_now{-1};
int64_t NowMs() {
    const int64_t fake = g_fake_now.load();
    return fake >= 0 ? fake : static_cast<int64_t>(GetTickCount64());
}

void FlushDenormals() { _mm_setcsr(_mm_getcsr() | 0x8040); } // FTZ | DAZ: RNNoise costs 3.7x without (§12.4)

double QpcUs() {
    static const double ticks_per_us = [] { LARGE_INTEGER f; QueryPerformanceFrequency(&f); return f.QuadPart / 1e6; }();
    LARGE_INTEGER c;
    QueryPerformanceCounter(&c);
    return c.QuadPart / ticks_per_us;
}

// ---------------------------------------------------------------- shared state

std::atomic<bool> g_running{false}, g_failed{false}, g_allowed{false}, g_ptt{false};
std::atomic<int> g_bundle{3};
std::atomic<int64_t> g_sending_until{0};
std::atomic<int> g_complexity{ComplexityController::kTop};
std::atomic<double> g_cost_override{-1.0}, g_cost_p95{0.0};
std::atomic<uint64_t> g_concealed{0};
std::atomic<testing::EchoTap> g_echo_tap{nullptr}; // the echo bench's view of each capture block (null in a game)
std::atomic<bool> g_mix_reference{true}; // false only in tests that loop a talker's own voice back (testing::SetMixReference)

// §15: the player's levels and chosen devices; they outlive an engine session (set by voice.cpp every 50 ms anyway).
std::atomic<float> g_mic_gain{1.0f};
struct SeatGains {
    std::atomic<float> gain[kSeats];
    SeatGains() { for (auto &g : gain) g.store(1.0f); }
} g_seat_gains;
std::mutex g_choice_lock;
std::wstring g_mic_choice, g_output_choice;           // IMMDevice ids; empty: the system default
std::atomic<uint32_t> g_mic_serial{0}, g_output_serial{0}; // bumped on a change: the open stream reopens

// NR-7: the lobby's voice mode and the spatial tuning; they outlive an engine session like the levels above.
std::atomic<uint8_t> g_voice_mode{static_cast<uint8_t>(spatial::Mode::Open)};
std::mutex g_params_lock;
spatial::Params g_params;           // Sanitized
std::atomic<uint32_t> g_params_serial{0};
spatial::Params SpatialParams() { std::lock_guard lock(g_params_lock); return g_params; }

std::wstring MicChoice() { std::lock_guard lock(g_choice_lock); return g_mic_choice; }
std::wstring OutputChoice() { std::lock_guard lock(g_choice_lock); return g_output_choice; }
// The render endpoint voice plays to now (the chosen output while it opens, else the default's id; empty: none open).
// The echo canceller's reference is this device's signal (the mix voice plays to it), so the two never differ.
std::wstring g_output_open;
std::wstring OutputInUse() { std::lock_guard lock(g_choice_lock); return g_output_open; }
void SetOutputInUse(const std::wstring &id) { std::lock_guard lock(g_choice_lock); g_output_open = id; }

float SafeGain(float g) { return std::isfinite(g) ? std::clamp(g, 0.0f, 1.0f) : 1.0f; }

// Codec-level failures: voice is off for the game session. Flags only (any thread, the game's included): the device
// workers see it within their next wait and the voice thread's next Start retires the session.
void Fail(const char *what) {
    if (!g_failed.exchange(true)) Log("voice: %s; voice chat is off for this session", what);
    g_running = false;
}

// ---------------------------------------------------------------- echo canceller and its far end

// Release 2.0.0, always on (OD-144): AEC3 between the far end (Render) and the microphone (Capture). One per session.
// The far end is the output's loopback (game + voice) while it delivers steadily, else the voice mix; exactly one of
// the two feeds AEC3 at a time, switched with hysteresis (the loopback after 1 s of steady data, back to the mix after
// 300 ms without), so a loopback that comes and goes never feeds AEC3 both references at once.
struct EchoLink {
    std::unique_ptr<echo::Canceller> canceller;
    std::atomic<bool> loopback_open{false};
    std::atomic<int64_t> loopback_data_ms{0}, loopback_since_ms{0};
    std::atomic<bool> use_loopback{false};
    int switches = 0; // the mixer's thread only

    // The mixer's thread, once per 20 ms frame: true while the loopback is the far end.
    bool UseLoopback() {
        const int64_t now = static_cast<int64_t>(GetTickCount64());
        const bool fresh = loopback_open.load() && now - loopback_data_ms.load() < 300;
        const bool was = use_loopback.load();
        bool use = was;
        if (was && !fresh) use = false;
        else if (!was && fresh && now - loopback_since_ms.load() >= 1000) use = true;
        if (use != was) {
            use_loopback.store(use);
            if (++switches <= 5 || switches % 50 == 0)
                Log("voice: echo reference is now the %s (switch %d)", use ? "output's loopback" : "voice mix", switches);
        }
        return use;
    }
    // The loopback worker: one 10 ms block of what the output plays (48 kHz mono).
    void LoopbackBlock(const float *block) {
        const int64_t now = static_cast<int64_t>(GetTickCount64());
        if (now - loopback_data_ms.load() > 300) loopback_since_ms.store(now);
        loopback_data_ms.store(now);
        if (use_loopback.load() && canceller) canceller->Render(block);
    }
};

// ---------------------------------------------------------------- capture chain

class CaptureChain {
public:
    CaptureChain(SendFn send, EchoLink *echo) : send_(std::move(send)), echo_(echo) {}
    ~CaptureChain() {
        if (ns_) rnnoise_destroy(ns_);
        if (enc_) opus_encoder_destroy(enc_);
        if (rp_) opus_repacketizer_destroy(rp_);
    }
    bool Init() {
        ns_ = rnnoise_create(nullptr);
        if (!ns_ || rnnoise_get_frame_size() != kSub) { Log("voice: RNNoise unavailable"); return false; }
        int err = 0;
        enc_ = opus_encoder_create(kRate, 1, OPUS_APPLICATION_VOIP, &err);
        if (err != OPUS_OK || !enc_) { Log("voice: Opus encoder unavailable (%d)", err); return false; }
        // §3.2 + §12.4: OPUS_SET_BANDWIDTH(WB) too, or 6 kbit/s falls to narrowband and NoLACE/deep PLC never run.
        const int ok = opus_encoder_ctl(enc_, OPUS_SET_BITRATE(6000)) | opus_encoder_ctl(enc_, OPUS_SET_VBR(1)) |
            opus_encoder_ctl(enc_, OPUS_SET_VBR_CONSTRAINT(1)) | opus_encoder_ctl(enc_, OPUS_SET_COMPLEXITY(10)) |
            opus_encoder_ctl(enc_, OPUS_SET_SIGNAL(OPUS_SIGNAL_VOICE)) | opus_encoder_ctl(enc_, OPUS_SET_INBAND_FEC(0)) |
            opus_encoder_ctl(enc_, OPUS_SET_DTX(0)) | opus_encoder_ctl(enc_, OPUS_SET_PACKET_LOSS_PERC(0)) |
            opus_encoder_ctl(enc_, OPUS_SET_MAX_BANDWIDTH(OPUS_BANDWIDTH_WIDEBAND)) |
            opus_encoder_ctl(enc_, OPUS_SET_BANDWIDTH(OPUS_BANDWIDTH_WIDEBAND));
        if (ok != OPUS_OK) { Log("voice: Opus encoder settings refused"); return false; }
        rp_ = opus_repacketizer_create();
        if (!rp_) { Log("voice: Opus repacketizer unavailable"); return false; }
        return true;
    }
    // A device buffer in `format`; `silent` buffers count as zeros.
    void FeedDevice(const dsp::DeviceFormat &format, const uint8_t *data, size_t frames, bool silent) {
        if (!(format == format_)) {
            format_ = format;
            resampler_.Reset(format.rate);
            Log("voice: capture %u Hz, %u channel(s), %s in %u-byte samples -> 48 kHz mono", format.rate, format.channels,
                dsp::KindName(format.kind), format.container);
        }
        if (silent || !data) mono_.assign(frames, 0.0f);
        else dsp::ToMono(format, data, frames, mono_);
        resampler_.Process(mono_.data(), mono_.size(), at48_);
        Feed48(at48_.data(), at48_.size());
    }
    // A capture gap (device reopened): the gate starts over.
    void Restart() {
        fill_ = 0;
        subs_ = 0;
        prev_sent_ = false;
    }

private:
    void Feed48(const float *x, size_t n) {
        for (size_t i = 0; i < n; i++) {
            raw_[fill_++] = x[i];
            if (fill_ == kSub) { fill_ = 0; CancelEcho(); Block(); }
        }
    }
    // Release 2.0.0: each raw 10 ms block through AEC3 and its 100 Hz high-pass (voice_echo.h) before the gain, RNNoise
    // and Opus. Without a canceller (never in a started engine) the 80 Hz high-pass alone, as before.
    void CancelEcho() {
        if (echo::Canceller *canceller = echo_ ? echo_->canceller.get() : nullptr) {
            const testing::EchoTap tap = g_echo_tap.load(std::memory_order_relaxed);
            float before[kSub];
            if (tap) std::memcpy(before, raw_, sizeof raw_);
            canceller->Capture(raw_);
            if (tap) tap(before, raw_, kSub);
            for (int i = 0; i < kSub; i++) sub_[i] = raw_[i] * 32768.0f;
            if (!echo_logged_ && ++echo_blocks_ % 100 == 0) { // once per session, when AEC3 has aligned to the far end
                if (const int delay = canceller->DelayMs(); delay >= 0) {
                    echo_logged_ = true;
                    Log("voice: echo canceller on (AEC3, delay %d ms)", delay);
                }
            }
        } else {
            for (int i = 0; i < kSub; i++) sub_[i] = hp_.Process(raw_[i]) * 32768.0f;
        }
    }
    void Block() {
        // transmit released (push-to-talk let go, muted, voice off) during a spurt: end it now, not at the bundle's end
        if (prev_sent_ && !g_allowed.load()) EndSpurt();
        if (subs_ == 0) bundle_ = std::clamp(g_bundle.load(), 3, kMaxBundle) == 6 ? 6 : 3; // a packet's size is fixed at its start
        agc_.Process(sub_, kSub, prev_voice_, kVoiceThreshold);
        // §15: "My microphone volume". After the AGC, which would undo any gain before it (echo cancellation works on
        // the raw capture, earlier still), and before RNNoise and the encoder: the voice that leaves the PC is this level.
        if (const float mic = g_mic_gain.load(std::memory_order_relaxed); mic != 1.0f)
            for (int i = 0; i < kSub; i++) sub_[i] *= mic;
        float clean[kSub];
        prev_voice_ = rnnoise_process_frame(ns_, clean, sub_);
        if (prev_voice_ >= kVoiceThreshold) since_voice_ = 0;
        else if (since_voice_ < 1000000) since_voice_++;
        if (since_voice_ <= kHangoverSubs) open_ = true;
        for (int i = 0; i < kSub; i++) pcm_[subs_ * kSub + i] = clean[i] / 32768.0f;
        if (++subs_ == 2 * bundle_) Datagram();
    }
    void Datagram() {
        const uint32_t ts = frames_;
        frames_ += static_cast<uint32_t>(bundle_);
        subs_ = 0;
        const bool allowed = g_allowed.load(), ptt = g_ptt.load();
        const bool send = allowed && (ptt || open_);
        open_ = false;
        if (!send) { prev_sent_ = false; return; }
        if (!prev_sent_) opus_encoder_ctl(enc_, OPUS_RESET_STATE); // a new spurt; the receiver resets at a ts gap
        // the last packet of a spurt: the hangover runs out before the next block, unless new voice reopens it
        const bool end = !ptt && since_voice_ + 1 > kHangoverSubs;
        prev_sent_ = true;
        EncodeAndSend(bundle_, ts, static_cast<uint8_t>((end ? 1 : 0) | (ptt ? 2 : 0)));
    }
    // The partial bundle, its last 10 ms padded with silence (one silent 20 ms frame if nothing is buffered), as a
    // packet of 1..bundle frames with the end flag: listeners stop at once instead of concealing.
    void EndSpurt() {
        const int frames = std::max(1, (subs_ + 1) / 2);
        std::fill(pcm_ + subs_ * kSub, pcm_ + frames * kFrame, 0.0f);
        const uint32_t ts = frames_;
        frames_ += static_cast<uint32_t>(frames);
        subs_ = 0;
        open_ = false;
        prev_sent_ = false;
        EncodeAndSend(frames, ts, static_cast<uint8_t>(1 | (g_ptt.load() ? 2 : 0)));
    }
    void EncodeAndSend(int frames, uint32_t ts, uint8_t flags) {
        opus_repacketizer_init(rp_);
        for (int k = 0; k < frames; k++) {
            const int len = opus_encode_float(enc_, pcm_ + k * kFrame, kFrame, frames_buf_[k], static_cast<opus_int32>(sizeof frames_buf_[k]));
            if (len < 1) { Fail("the Opus encoder failed"); return; }
            if (opus_repacketizer_cat(rp_, frames_buf_[k], len) != OPUS_OK) { Fail("the Opus repacketizer failed"); return; }
        }
        const opus_int32 size = opus_repacketizer_out(rp_, packet_, static_cast<opus_int32>(sizeof packet_));
        if (size < 1) { Fail("the Opus repacketizer failed"); return; }
        g_sending_until = (flags & 1) ? 0 : NowMs() + 20LL * frames + 60;
        try {
            send_(packet_, static_cast<size_t>(size), flags, seq_++, ts);
        } catch (...) {
            Fail("the voice sender threw");
        }
    }

    SendFn send_;
    EchoLink *echo_ = nullptr;
    DenoiseState *ns_ = nullptr;
    OpusEncoder *enc_ = nullptr;
    OpusRepacketizer *rp_ = nullptr;
    dsp::DeviceFormat format_{0, 0, dsp::SampleKind::Float32, 0};
    dsp::Resampler resampler_;
    dsp::HighPass hp_;
    dsp::Agc agc_;
    std::vector<float> mono_, at48_;
    float raw_[kSub] = {}, sub_[kSub] = {};
    int echo_blocks_ = 0;
    bool echo_logged_ = false;
    float pcm_[kMaxBundle * kFrame] = {};
    unsigned char frames_buf_[kMaxBundle][400] = {};
    unsigned char packet_[1500] = {};
    int fill_ = 0, subs_ = 0, bundle_ = 3, since_voice_ = 1000000;
    float prev_voice_ = 0.0f;
    bool open_ = false, prev_sent_ = false;
    uint16_t seq_ = 0;
    uint32_t frames_ = 0;
};

// ---------------------------------------------------------------- playback: jitter buffers, decoders, mix

struct Packet {
    uint8_t seat = 0, flags = 0;
    uint16_t seq = 0;
    uint32_t ts = 0;
    int frames = 0;
    std::vector<uint8_t> data;
};

int32_t TsDiff(uint32_t a, uint32_t b) { return static_cast<int32_t>(a - b); }

struct Seat {
    OpusDecoder *dec = nullptr;
    std::deque<Packet> buffer; // ordered by ts
    bool playing = false, ended = false;
    uint32_t next_ts = 0;
    int target = 1, waited = 0, concealed_run = 0, stable = 0;
    float pcm[kMaxBundle * kFrame] = {};
    int pcm_len = 0, pcm_pos = 0;
    void Clear() {
        buffer.clear();
        playing = ended = false;
        pcm_len = pcm_pos = 0;
        waited = concealed_run = 0;
    }
};

class Mixer {
public:
    explicit Mixer(EchoLink *echo) : echo_(echo) {}
    ~Mixer() {
        for (auto &s : seats_) if (s.dec) opus_decoder_destroy(s.dec);
    }
    bool Init() { // a decoder at the chosen complexity proves the library (OSCE/deep PLC compiled in)
        int err = 0;
        OpusDecoder *probe = opus_decoder_create(kRate, 1, &err);
        if (err != OPUS_OK || !probe) { Log("voice: Opus decoder unavailable (%d)", err); return false; }
        bool ok = true;
        for (int level = ComplexityController::kBottom; level <= ComplexityController::kTop; level++)
            ok = ok && opus_decoder_ctl(probe, OPUS_SET_COMPLEXITY(level)) == OPUS_OK;
        opus_decoder_destroy(probe);
        if (!ok) Log("voice: Opus decoder complexities 5-7 refused");
        g_complexity = control_.Level();
        return ok;
    }
    void Receive(uint8_t seat, uint16_t seq, uint32_t ts, uint8_t flags, const uint8_t *opus, size_t size) {
        if (seat >= kSeats) { // the relay's voice seats are 0-11 (MATCHMAKING_API.md §5c): said once, never silently
            if (!seat_logged_.exchange(true)) Log("voice: a packet for seat %u (beyond %d) is not played", seat, kSeats - 1);
            return;
        }
        if (!opus || size < 1 || size > 400 || silenced_[seat].load()) return;
        const int samples = opus_packet_get_nb_samples(opus, static_cast<opus_int32>(size), kRate);
        if (samples < kFrame || samples > kMaxBundle * kFrame || samples % kFrame) return; // only whole 20 ms frames
        Packet p;
        p.seat = seat; p.seq = seq; p.ts = ts; p.flags = flags; p.frames = samples / kFrame;
        p.data.assign(opus, opus + size);
        last_rx_[seat] = NowMs();
        std::lock_guard lock(inbox_mutex_);
        if (inbox_.size() < kMaxInbox) inbox_.push_back(std::move(p));
    }
    void SetSilenced(uint8_t seat, bool silenced) { if (seat < kSeats) silenced_[seat] = silenced; }
    bool Talking(uint8_t seat) const {
        return seat < kSeats && !silenced_[seat].load() && NowMs() - last_rx_[seat].load() <= kTalkingMs;
    }
    // Talking and audible now: in proximity a talker beyond range is talking but not heard (gain under -60 dB).
    bool Heard(uint8_t seat) const { return Talking(seat) && audible_[seat].load(std::memory_order_relaxed) > 1e-3f; }
    void SetVolume(float v) { volume_ = std::clamp(std::isfinite(v) ? v : 0.0f, 0.0f, 1.0f); }
    // NR-7: the poses of a voice down, stamped with the audio clock (samples mixed so far): applied at the next frame.
    void SetPose(uint8_t seat, const spatial::Pose &talker, const spatial::Pose &self) {
        if (seat >= kSeats) return;
        const double arrived = static_cast<double>(mixed_.load()) / kRate;
        std::lock_guard lock(pose_mutex_);
        if (poses_.size() < kMaxInbox) poses_.push_back({seat, talker, self, arrived});
    }
    // `frames` stereo frames, interleaved L, R.
    size_t Pull(float *out, size_t frames) {
        size_t done = 0;
        while (done < frames) {
            if (mix_pos_ == kFrame) { MixFrame(mix_); mix_pos_ = 0; }
            const size_t take = std::min(frames - done, static_cast<size_t>(kFrame - mix_pos_));
            std::memcpy(out + 2 * done, mix_ + 2 * mix_pos_, 2 * take * sizeof(float));
            mix_pos_ += static_cast<int>(take);
            done += take;
        }
        return done;
    }

private:
    void DrainInbox() {
        std::vector<Packet> in;
        { std::lock_guard lock(inbox_mutex_); in.swap(inbox_); }
        for (auto &p : in) {
            Seat &s = seats_[p.seat];
            if (silenced_[p.seat].load()) continue;
            if (s.playing && TsDiff(p.ts + static_cast<uint32_t>(p.frames), s.next_ts) <= 0) continue; // all of it too late
            auto at = std::find_if(s.buffer.begin(), s.buffer.end(), [&](const Packet &q) { return TsDiff(q.ts, p.ts) >= 0; });
            if (at != s.buffer.end() && at->ts == p.ts) continue; // duplicate
            s.buffer.insert(at, std::move(p));
            while (s.buffer.size() > kMaxBuffered) s.buffer.pop_front(); // keep latency bounded
        }
    }
    bool EnsureDecoder(Seat &s) {
        if (s.dec) return true;
        int err = 0;
        s.dec = opus_decoder_create(kRate, 1, &err);
        if (err != OPUS_OK || !s.dec || opus_decoder_ctl(s.dec, OPUS_SET_COMPLEXITY(control_.Level())) != OPUS_OK) {
            Fail("an Opus decoder could not be created");
            return false;
        }
        return true;
    }
    // Refills `s.pcm` with the next decoded or concealed audio; false when the seat has nothing to play now.
    bool Refill(Seat &s) {
        for (int guard = 0; guard < 8; guard++) {
            if (!s.playing) {
                if (s.buffer.empty()) return false;
                const bool ready = s.buffer.size() >= static_cast<size_t>(s.target) || (s.buffer.back().flags & 1) ||
                    ++s.waited >= 3 * s.target; // never wait more than ~60 ms per target packet
                if (!ready) return false;
                if (!EnsureDecoder(s)) return false;
                opus_decoder_ctl(s.dec, OPUS_RESET_STATE); // a new talk spurt (the encoder reset too)
                s.playing = true;
                s.ended = false;
                s.waited = s.concealed_run = 0;
                s.next_ts = s.buffer.front().ts;
            }
            while (!s.buffer.empty() && TsDiff(s.buffer.front().ts + static_cast<uint32_t>(s.buffer.front().frames), s.next_ts) <= 0)
                s.buffer.pop_front();
            // the expected packet, or one that arrived while its first frames were being concealed
            if (!s.buffer.empty() && TsDiff(s.buffer.front().ts, s.next_ts) <= 0) {
                Packet p = std::move(s.buffer.front());
                s.buffer.pop_front();
                decodes_ += p.frames;
                const int got = opus_decode_float(s.dec, p.data.data(), static_cast<opus_int32>(p.data.size()), s.pcm, p.frames * kFrame, 0);
                const uint32_t end_ts = p.ts + static_cast<uint32_t>(p.frames);
                const int skip = TsDiff(s.next_ts, p.ts) * kFrame; // frames already concealed
                s.next_ts = end_ts;
                if (got <= skip) continue; // a corrupt packet: its frames are concealed instead
                s.pcm_len = got;
                s.pcm_pos = skip;
                s.ended = (p.flags & 1) != 0;
                s.concealed_run = 0;
                if (++s.stable > 500 && s.target > 1) { s.target = 1; s.stable = 0; } // 30 s without an underrun
                return true;
            }
            // the expected packet is missing
            if (s.ended) { s.playing = false; continue; }        // the spurt ended: a later packet is a new spurt
            if (s.buffer.empty()) {                              // underrun
                if (s.concealed_run == 0) { s.target = 2; s.stable = 0; }
                if (s.concealed_run >= kMaxConcealFrames) { s.playing = false; return false; }
            }
            decodes_++;
            const int got = opus_decode_float(s.dec, nullptr, 0, s.pcm, kFrame, 0); // deep PLC
            s.next_ts += 1;
            s.concealed_run++;
            g_concealed++;
            if (got <= 0) return false;
            s.pcm_len = got;
            s.pcm_pos = 0;
            return true;
        }
        return false;
    }
    // NR-7: the poses that arrived since the last frame, the mode and the tuning, for this frame's seats.
    void ApplyPoses() {
        std::vector<PoseUpdate> in;
        { std::lock_guard lock(pose_mutex_); in.swap(poses_); }
        for (const PoseUpdate &u : in) {
            talkers_[u.seat].Update(u.talker, u.arrived);
            scene_.SetListener(u.self, u.arrived);
        }
        if (const uint32_t serial = g_params_serial.load(); serial != params_serial_) { params_serial_ = serial; scene_.params = SpatialParams(); }
        scene_.proximity = static_cast<spatial::Mode>(g_voice_mode.load(std::memory_order_relaxed)) == spatial::Mode::Proximity;
    }
    // One 20 ms stereo frame (interleaved). Each seat's decoded voice (its volume applied) goes through its
    // SeatProcessor: in open mode (and in proximity while this player has no car) that adds exactly the old mono
    // signal to both channels, so open mode is the old mix sample for sample.
    void MixFrame(float *out) {
        DrainInbox();
        ApplyPoses();
        std::fill(out, out + 2 * kFrame, 0.0f);
        const float volume = volume_.load();
        decodes_ = 0;
        int talkers = 0;
        const double started = QpcUs();
        for (int i = 0; i < kSeats; i++) {
            Seat &s = seats_[i];
            filled_[i] = 0;
            if (silenced_[i].load()) {
                if (s.playing || !s.buffer.empty() || s.pcm_len) s.Clear();
                if (spatial_[i].Busy()) spatial_[i].Reset(); // silent at once, not after the delay line's tail
                continue;
            }
            int filled = 0;
            const int decodes_before = decodes_;
            const float gain = volume * g_seat_gains.gain[i].load(std::memory_order_relaxed); // §15: this talker's volume
            float *in = voice_[i].data();
            while (filled < kFrame) {
                if (s.pcm_pos >= s.pcm_len && !Refill(s)) break;
                const int take = std::min(kFrame - filled, s.pcm_len - s.pcm_pos);
                for (int k = 0; k < take; k++) in[filled + k] = s.pcm[s.pcm_pos + k] * gain;
                s.pcm_pos += take;
                filled += take;
            }
            filled_[i] = filled;
            if (filled || decodes_ != decodes_before) talkers++;
        }
        const double cost = QpcUs() - started; // decoding only: the complexity rule's input, as before
        const double t0 = static_cast<double>(mixed_.load()) / kRate;
        for (int i = 0; i < kSeats; i++) {
            if (silenced_[i].load()) continue;
            spatial_[i].Process(voice_[i].data(), filled_[i], kFrame, out, t0, scene_, talkers_[i]);
            audible_[i].store(spatial_[i].AudibleGain(), std::memory_order_relaxed);
        }
        mixed_ += kFrame;
        for (int k = 0; k < 2 * kFrame; k++) out[k] = std::clamp(out[k], -1.0f, 1.0f);
        // Release 2.0.0: what will be played is the echo canceller's far end (silence too: AEC3 wants every block),
        // unless the output's loopback (game + this mix) is feeding it. NR-7: the final stereo mix as (L + R) / 2, as the
        // loopback path's ToMono does; in open mode that is the old mono mix exactly.
        if (echo::Canceller *canceller = echo_ ? echo_->canceller.get() : nullptr) {
            const bool loopback = echo_->UseLoopback();
            if (g_mix_reference.load(std::memory_order_relaxed) && !loopback) {
                float mono[kFrame];
                for (int k = 0; k < kFrame; k++) mono[k] = (out[2 * k] + out[2 * k + 1]) * 0.5f;
                canceller->Render(mono);
                canceller->Render(mono + kSub);
            }
        }
        // A packet decodes 3 (or 6) frames at once, so the raw time per mix frame is bursty. The load is the cost per
        // 20 ms of audio times the talkers, reported once per 20 ms of audio decoded: the controller's frames are
        // then 20 ms of talking time whatever the bundle.
        if (decodes_) {
            const int talking = std::max(talkers, 1);
            const double load = g_cost_override.load() >= 0 ? g_cost_override.load() : cost * talking / decodes_;
            for (int k = std::max(1, (decodes_ + talking / 2) / talking); k > 0; k--) Adapt(load);
        }
    }
    // The measured decode cost of this frame (all talkers) drives the one adaptive rule.
    void Adapt(double cost_us) {
        const bool changed = control_.OnFrame(cost_us);
        g_cost_p95 = control_.CurrentP95();
        if (!changed) return;
        const auto &c = control_.Last();
        if (c.to < c.from)
            Log("voice: decoder complexity %d -> %d: decode p95 %.0f us per 20 ms over the %.0f us budget", c.from, c.to, c.p95_us, control_.Budget());
        else
            Log("voice: decoder complexity %d -> %d: p95 %.0f us at %d, projected %.0f us at %d (budget %.0f us)", c.from, c.to, c.p95_us, c.from, c.projected_us, c.to, control_.Budget());
        for (auto &s : seats_)
            if (s.dec && opus_decoder_ctl(s.dec, OPUS_SET_COMPLEXITY(c.to)) != OPUS_OK) { Fail("decoder complexity change refused"); return; }
        g_complexity = c.to;
    }

    EchoLink *echo_ = nullptr;
    ComplexityController control_;
    int decodes_ = 0; // 20 ms frames decoded or concealed in this mix frame
    std::array<Seat, kSeats> seats_;
    std::array<std::atomic<bool>, kSeats> silenced_{};
    std::array<std::atomic<int64_t>, kSeats> last_rx_{};
    std::atomic<bool> seat_logged_{false};
    std::atomic<float> volume_{1.0f};
    std::mutex inbox_mutex_;
    std::vector<Packet> inbox_;
    float mix_[2 * kFrame] = {}; // stereo, interleaved
    int mix_pos_ = kFrame;
    // NR-7: per seat its decoded voice of this frame, its spatial processing (a 128 KB delay line each, made at Start)
    // and its car; the listener; the audio clock.
    struct PoseUpdate {
        uint8_t seat;
        spatial::Pose talker, self;
        double arrived;
    };
    std::array<std::array<float, kFrame>, kSeats> voice_{};
    std::array<int, kSeats> filled_{};
    std::array<spatial::SeatProcessor, kSeats> spatial_;
    std::array<spatial::Track, kSeats> talkers_;
    std::array<std::atomic<float>, kSeats> audible_{};
    spatial::Scene scene_;
    uint32_t params_serial_ = ~0u;
    std::atomic<uint64_t> mixed_{0}; // samples mixed
    std::mutex pose_mutex_;
    std::vector<PoseUpdate> poses_;
};

// ---------------------------------------------------------------- engine lifetime

// One engine session: the chains, the echo canceller and the device workers that feed them. Shared by the engine (the
// published pointer) and every worker (its own reference), so a worker left behind in a hung driver call never has
// anything freed under it. The workers: capture, playback, the echo reference's loopback, and the watchdog.
enum Role : int { kCaptureRole, kPlaybackRole, kLoopbackRole, kRoles, kWatchdogRole = kRoles };
const char *const kRoleNames[kRoles] = {"microphone", "audio output", "output loopback"};
constexpr int kMaxAbandon = 3;   // per role and session; then that part of voice stays off
constexpr int kMaxWorkers = 16;  // alive in the process, left-behind ones included: no new session beyond it

std::atomic<int> g_workers_alive{0};
std::atomic<int> g_stuck_ms{8000};
std::atomic<testing::FakeDevices *> g_fake{nullptr};

struct RoleState {
    std::atomic<uint32_t> gen{0};          // bumped when a stuck worker is abandoned: the old one is no longer current
    std::atomic<int64_t> busy_since{0};    // when the current worker left its last wait (0: waiting, or none)
    std::atomic<int> abandoned{0};
    std::atomic<bool> active{false};       // a worker should be running for this role
};

struct Session {
    std::atomic<bool> running{true};
    HANDLE stop = nullptr;                 // manual reset: Stop, or a failed Start
    EchoLink echo;                         // before the chains: they point at it
    std::unique_ptr<CaptureChain> capture;
    std::unique_ptr<Mixer> mixer;
    std::mutex capture_lock, mix_lock;     // one worker at a time feeds the chain / pulls the mix (a late one skips)
    RoleState roles[kRoles];
    HANDLE first_try[2] = {};              // set after the capture's and the playback's first device attempt
    std::atomic<int> device_ok[2] = {};    // 1 opened, 2 failed
    std::mutex threads_lock;
    std::vector<HANDLE> threads;
    Session() = default;
    Session(const Session &) = delete;
    Session &operator=(const Session &) = delete;
    ~Session() {
        if (stop) CloseHandle(stop);
        for (HANDLE e : first_try) if (e) CloseHandle(e);
        for (HANDLE t : threads) CloseHandle(t);
    }
};

// exclusive: only to publish or take the session pointer (never across a device call or a wait); shared: every call
// that uses the session, with a try on every path the game's threads take
std::shared_mutex g_life;
std::shared_ptr<Session> g_session;
std::atomic<bool> g_mic_ok{false};    // the last capture attempt opened a microphone (false: listen-only)

struct Worker {
    std::shared_ptr<Session> s;
    int role;
    uint32_t gen;
    DWORD start_delay_ms;
};

int64_t RealMs() { return static_cast<int64_t>(GetTickCount64()); }
bool Current(const Worker &w) { return w.role >= kRoles || w.s->roles[w.role].gen.load() == w.gen; }
bool Live(const Worker &w) { return w.s->running.load() && !g_failed.load() && Current(w); }
void Busy(const Worker &w) { if (w.role < kRoles && Current(w)) w.s->roles[w.role].busy_since.store(RealMs()); }
void Idle(const Worker &w) { if (w.role < kRoles && Current(w)) w.s->roles[w.role].busy_since.store(0); }
// A wait the watchdog does not count as a stuck device call.
DWORD WaitIdle(const Worker &w, DWORD count, const HANDLE *handles, DWORD ms) {
    Idle(w);
    const DWORD r = WaitForMultipleObjects(count, handles, FALSE, ms);
    Busy(w);
    return r;
}

template <typename T> void SafeRelease(T *&p) { if (p) { p->Release(); p = nullptr; } }

std::string Utf8(const std::wstring &w) {
    if (w.empty()) return {};
    const int n = WideCharToMultiByte(CP_UTF8, 0, w.c_str(), static_cast<int>(w.size()), nullptr, 0, nullptr, nullptr);
    std::string s(n > 0 ? n : 0, '\0');
    if (n > 0) WideCharToMultiByte(CP_UTF8, 0, w.c_str(), static_cast<int>(w.size()), s.data(), n, nullptr, nullptr);
    return s;
}

std::string FormatText(const dsp::DeviceFormat &f) {
    char text[96];
    snprintf(text, sizeof text, "%u Hz, %u channel(s), %s in %u-byte samples", f.rate, f.channels, dsp::KindName(f.kind), f.container);
    return text;
}

// PKEY_Device_FriendlyName ({a45c254e-df1c-4efd-8020-67d146a850e0}, 14), spelled out: no INITGUID and no extra lib.
constexpr PROPERTYKEY kFriendlyName = {{0xa45c254e, 0xdf1c, 0x4efd, {0x80, 0x20, 0x67, 0xd1, 0x46, 0xa8, 0x50, 0xe0}}, 14};

std::wstring FriendlyName(IMMDevice *device) {
    std::wstring name;
    IPropertyStore *store = nullptr;
    if (SUCCEEDED(device->OpenPropertyStore(STGM_READ, &store))) {
        PROPVARIANT value;
        PropVariantInit(&value);
        if (SUCCEEDED(store->GetValue(kFriendlyName, &value)) && value.vt == VT_LPWSTR && value.pwszVal) name = value.pwszVal;
        PropVariantClear(&value);
    }
    SafeRelease(store);
    return name;
}

std::vector<std::wstring> EndpointIds(IMMDeviceEnumerator *enumerator, EDataFlow flow, std::vector<IMMDevice *> *devices) {
    std::vector<std::wstring> ids;
    IMMDeviceCollection *all = nullptr;
    UINT count = 0;
    if (SUCCEEDED(enumerator->EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE, &all)) && SUCCEEDED(all->GetCount(&count)))
        for (UINT i = 0; i < count; i++) {
            IMMDevice *device = nullptr;
            LPWSTR id = nullptr;
            if (SUCCEEDED(all->Item(i, &device)) && SUCCEEDED(device->GetId(&id))) {
                ids.push_back(id);
                if (devices) { devices->push_back(device); device = nullptr; }
            }
            if (id) CoTaskMemFree(id);
            SafeRelease(device);
        }
    SafeRelease(all);
    return ids;
}

// The active capture endpoints as voice_audio_devices.h sees them, and the default communications input's index (-1:
// none, or the default is not one of the enumerated endpoints: then it is never opened). `logged` (the capture
// worker's own) logs each rejected endpoint once, by kind only: a friendly name can carry a person's name ("...'s
// AirPods"); no audio is ever logged. null logs nothing.
std::vector<devices::Candidate> CaptureCandidates(IMMDeviceEnumerator *enumerator, int &default_index, std::vector<std::wstring> *logged) {
    std::vector<IMMDevice *> devices, outputs;
    const std::vector<std::wstring> ids = EndpointIds(enumerator, eCapture, &devices);
    const std::vector<std::wstring> render = EndpointIds(enumerator, eRender, &outputs);
    std::vector<std::wstring> render_names; // a capture named after one is its monitor (devices::NamesAnOutput)
    for (IMMDevice *&output : outputs) { render_names.push_back(FriendlyName(output)); SafeRelease(output); }
    std::wstring default_id;
    IMMDevice *def = nullptr;
    LPWSTR def_id = nullptr;
    if (SUCCEEDED(enumerator->GetDefaultAudioEndpoint(eCapture, eCommunications, &def)) && SUCCEEDED(def->GetId(&def_id))) default_id = def_id;
    if (def_id) CoTaskMemFree(def_id);
    SafeRelease(def);
    std::vector<devices::Candidate> list;
    default_index = -1;
    for (size_t i = 0; i < ids.size(); i++) {
        devices::Candidate c{ids[i], FriendlyName(devices[i]), std::find(render.begin(), render.end(), ids[i]) != render.end()};
        c.monitor = devices::NamesAnOutput(c.name, render_names);
        IMMEndpoint *endpoint = nullptr;
        EDataFlow flow = eCapture;
        if (SUCCEEDED(devices[i]->QueryInterface(__uuidof(IMMEndpoint), reinterpret_cast<void **>(&endpoint))) &&
            SUCCEEDED(endpoint->GetDataFlow(&flow)) && flow != eCapture)
            c.render = true;
        SafeRelease(endpoint);
        SafeRelease(devices[i]);
        if (ids[i] == default_id) default_index = static_cast<int>(i);
        if (logged && devices::IsMonitorOrLoopback(c) && std::find(logged->begin(), logged->end(), c.id) == logged->end()) {
            logged->push_back(c.id);
            Log("voice: capture endpoint %zu%s is %s; not used", i, ids[i] == default_id ? " (the default)" : "",
                devices::IsDefaultAlias(c) ? "the sound server's default stand-in (it may be a monitor)" : "a monitor/loopback");
        }
        list.push_back(std::move(c));
    }
    return list;
}

// The capture endpoint voice may use (voice_audio_devices.h): the player's chosen microphone while it is there, else the
// default or first microphone; empty: only monitors/loopbacks/default stand-ins, or no input at all.
std::wstring PickFrom(const std::vector<devices::Candidate> &list, int default_index) {
    const std::wstring chosen = MicChoice();
    thread_local std::wstring missing_logged; // per capture worker
    if (devices::ChosenMissing(list, chosen, true)) {
        if (missing_logged != chosen) { missing_logged = chosen; Log("voice: the chosen microphone is not there (or is not a microphone); using the system default"); }
    } else {
        missing_logged.clear();
    }
    const int pick = devices::PickCapture(list, default_index, chosen);
    return pick < 0 ? std::wstring() : list[pick].id;
}

std::wstring SelectCapture(IMMDeviceEnumerator *enumerator) {
    thread_local std::vector<std::wstring> logged; // per capture worker
    int default_index = -1;
    const std::vector<devices::Candidate> list = CaptureCandidates(enumerator, default_index, &logged);
    return PickFrom(list, default_index);
}

// The active render endpoints (devices::Candidate, render set), for the output's list and choice.
std::vector<devices::Candidate> RenderCandidates(IMMDeviceEnumerator *enumerator) {
    std::vector<IMMDevice *> devices;
    const std::vector<std::wstring> ids = EndpointIds(enumerator, eRender, &devices);
    std::vector<devices::Candidate> list;
    for (size_t i = 0; i < ids.size(); i++) {
        list.push_back({ids[i], FriendlyName(devices[i]), true});
        SafeRelease(devices[i]);
    }
    return list;
}

// The output to open: the player's choice while it is active, else empty (the default output).
std::wstring SelectOutput(IMMDeviceEnumerator *enumerator) {
    const std::wstring chosen = OutputChoice();
    if (chosen.empty() || !enumerator) return {};
    const std::vector<devices::Candidate> list = RenderCandidates(enumerator);
    return devices::FindChosen(list, chosen, false) >= 0 ? chosen : std::wstring();
}

// The default output's id (eConsole), or empty: polled to follow a changed or lost output on both platforms.
std::wstring DefaultRenderId(IMMDeviceEnumerator *enumerator) {
    std::wstring result;
    IMMDevice *device = nullptr;
    LPWSTR id = nullptr;
    if (enumerator && SUCCEEDED(enumerator->GetDefaultAudioEndpoint(eRender, eConsole, &device)) && SUCCEEDED(device->GetId(&id))) result = id;
    if (id) CoTaskMemFree(id);
    SafeRelease(device);
    return result;
}

void FirstTry(Session &s, int which, bool ok) {
    if (s.device_ok[which].load() == 0) { s.device_ok[which] = ok ? 1 : 2; if (s.first_try[which]) SetEvent(s.first_try[which]); }
    else if (ok) s.device_ok[which] = 1;
}

bool ParseFormat(const WAVEFORMATEX *w, dsp::DeviceFormat &f) {
    if (!w || !w->nChannels || w->nChannels > 32 || w->nSamplesPerSec < 4000 || w->nSamplesPerSec > 384000) return false;
    WORD tag = w->wFormatTag, valid = w->wBitsPerSample;
    if (tag == WAVE_FORMAT_EXTENSIBLE) {
        if (w->cbSize < 22) return false;
        const auto *x = reinterpret_cast<const WAVEFORMATEXTENSIBLE *>(w);
        tag = static_cast<WORD>(x->SubFormat.Data1); // KSDATAFORMAT_SUBTYPE_PCM = 1, _IEEE_FLOAT = 3
        if (x->Samples.wValidBitsPerSample) valid = x->Samples.wValidBitsPerSample;
    }
    const WORD bits = w->wBitsPerSample;
    f.rate = w->nSamplesPerSec;
    f.channels = w->nChannels;
    f.container = static_cast<uint16_t>(bits / 8);
    if (bits % 8 || w->nBlockAlign != f.container * f.channels) return false;
    if (tag == 3 && bits == 32) f.kind = dsp::SampleKind::Float32;
    else if (tag == 3 && bits == 64) f.kind = dsp::SampleKind::Float64;
    else if (tag == 1 && bits == 8) f.kind = dsp::SampleKind::UInt8;
    else if (tag == 1 && bits == 16) f.kind = dsp::SampleKind::Int16;
    else if (tag == 1 && bits == 24) f.kind = dsp::SampleKind::Int24;
    else if (tag == 1 && bits == 32) f.kind = valid > 16 && valid <= 24 ? dsp::SampleKind::Int24 : dsp::SampleKind::Int32;
    else return false;
    return true;
}

enum class Outcome { Reopen, Retry, RetrySlow }; // reopen at once; retry in 2 s; retry with backoff (2..16 s)

// One device buffer into the chain, only while this worker is the current one (a worker abandoned in a driver call
// that comes back late feeds nothing).
bool Feed(const Worker &w, const dsp::DeviceFormat &f, const uint8_t *data, size_t frames, bool silent) {
    std::lock_guard lock(w.s->capture_lock);
    if (!Live(w)) return false;
    w.s->capture->FeedDevice(f, data, frames, silent);
    return true;
}

void RestartCapture(const Worker &w) {
    std::lock_guard lock(w.s->capture_lock);
    if (Live(w)) w.s->capture->Restart();
}

void MicState(const Worker &w, bool ok) {
    if (!Live(w)) return;
    g_mic_ok = ok;
    FirstTry(*w.s, 0, ok);
}

// A microphone that delivers at another rate than its format says, seen again after a reopen: that rate is used (the
// capture worker's memory; a new format or device forgets it).
struct RateMemo {
    dsp::DeviceFormat nominal{0, 0, dsp::SampleKind::Float32, 0};
    uint32_t measured = 0;
    int strikes = 0;
};
thread_local RateMemo t_rate;

// One open capture session; returns when stopped, the device fails or must be reopened.
Outcome CaptureSession(const Worker &w, HANDLE ready) {
    Session &s = *w.s;
    const uint32_t serial = g_mic_serial.load();
    IMMDeviceEnumerator *enumerator = nullptr;
    IMMDevice *device = nullptr;
    IAudioClient *client = nullptr;
    IAudioCaptureClient *capture = nullptr;
    WAVEFORMATEX *mix = nullptr;
    dsp::DeviceFormat format;
    std::wstring chosen;
    HRESULT hr = CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL, IID_PPV_ARGS(&enumerator));
    if (SUCCEEDED(hr)) { chosen = SelectCapture(enumerator); if (chosen.empty()) hr = HRESULT_FROM_WIN32(ERROR_NOT_FOUND); }
    if (SUCCEEDED(hr)) hr = enumerator->GetDevice(chosen.c_str(), &device);
    if (SUCCEEDED(hr)) hr = device->Activate(__uuidof(IAudioClient), CLSCTX_ALL, nullptr, reinterpret_cast<void **>(&client));
    if (SUCCEEDED(hr)) hr = client->GetMixFormat(&mix);
    if (SUCCEEDED(hr) && !ParseFormat(mix, format)) hr = E_NOTIMPL;
    // 100 ms of device buffer: a worker held up (scheduling, a slow block) loses nothing
    if (SUCCEEDED(hr)) hr = client->Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 1000000, 0, mix, nullptr);
    if (SUCCEEDED(hr)) hr = client->SetEventHandle(ready);
    if (SUCCEEDED(hr)) hr = client->GetService(IID_PPV_ARGS(&capture));
    if (SUCCEEDED(hr)) hr = client->Start();
    thread_local HRESULT last_error = S_OK;
    Outcome outcome = Outcome::Retry;
    if (FAILED(hr)) {
        if (hr != last_error) {
            if (hr == HRESULT_FROM_WIN32(ERROR_NOT_FOUND)) Log("voice: no microphone (no input, or only monitor/loopback inputs); listening only, checking every 2 s");
            else Log("voice: no microphone (0x%08lx); retrying", static_cast<unsigned long>(hr));
        }
        last_error = hr;
        MicState(w, false);
        outcome = hr == HRESULT_FROM_WIN32(ERROR_NOT_FOUND) ? Outcome::Retry : Outcome::RetrySlow;
    } else {
        last_error = S_OK;
        MicState(w, true);
        dsp::DeviceFormat effective = format;
        if (t_rate.strikes >= 2 && t_rate.nominal == format) effective.rate = t_rate.measured;
        Log("voice: microphone open (%s%s)", FormatText(format).c_str(), effective.rate != format.rate ? "; its data is resampled from the rate it really delivers" : "");
        RestartCapture(w);
        dsp::RateWatch watch(effective.rate);
        ULONGLONG last_check = GetTickCount64(), last_data = last_check;
        const size_t stride = static_cast<size_t>(format.container) * format.channels;
        std::vector<uint8_t> packet_copy; // the chain runs on a copy: the device buffer is released at once
        bool done = false;
        while (!done) {
            HANDLE waits[2] = {s.stop, ready};
            if (WaitIdle(w, 2, waits, 500) == WAIT_OBJECT_0 || !Live(w)) break;
            UINT32 packet = 0;
            while (SUCCEEDED(hr = capture->GetNextPacketSize(&packet)) && packet) {
                last_data = GetTickCount64();
                BYTE *data = nullptr;
                UINT32 frames = 0;
                DWORD flags = 0;
                hr = capture->GetBuffer(&data, &frames, &flags, nullptr, nullptr);
                if (FAILED(hr)) break;
                const bool silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT) != 0;
                if (!silent && frames) packet_copy.assign(data, data + static_cast<size_t>(frames) * stride);
                capture->ReleaseBuffer(frames);
                // AEC3, RNNoise, Opus and the send run after the release, so they never hold the device buffer
                if (!Feed(w, effective, silent ? nullptr : packet_copy.data(), frames, silent)) { done = true; break; }
                if (const uint32_t measured = watch.Add(frames, RealMs(), (flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY) != 0)) {
                    // a Bluetooth profile flip or a relabelled stream: reopen; the same mismatch again: use that rate
                    if (t_rate.nominal == format && t_rate.measured == measured) t_rate.strikes++;
                    else t_rate = {format, measured, 1};
                    Log("voice: the microphone delivers about %.0f Hz while its format says %u Hz; reopening", watch.LastMeasured(), effective.rate);
                    outcome = Outcome::Reopen;
                    done = true;
                    break;
                }
            }
            if (done) break;
            if (FAILED(hr)) {
                if (hr == AUDCLNT_E_DEVICE_INVALIDATED) { Log("voice: microphone lost or its format changed; reopening"); outcome = Outcome::Reopen; }
                else Log("voice: microphone lost (0x%08lx); retrying", static_cast<unsigned long>(hr));
                break;
            }
            // a live shared-mode stream always delivers (silence too); none for 2 s is a lost device that did not say
            // so (Wine/PulseAudio may not report AUDCLNT_E_DEVICE_INVALIDATED)
            if (GetTickCount64() - last_data > 2000) { Log("voice: microphone stalled; reopening"); break; }
            if (g_mic_serial.load() != serial) { Log("voice: the player chose another microphone; reopening"); outcome = Outcome::Reopen; break; } // §15
            if (GetTickCount64() - last_check > 3000) { // the microphone to use changed (the default, or one plugged in)
                last_check = GetTickCount64();
                if (SelectCapture(enumerator) != chosen) { Log("voice: microphone changed; reopening"); outcome = Outcome::Reopen; break; }
            }
        }
        client->Stop();
    }
    if (mix) CoTaskMemFree(mix);
    SafeRelease(capture);
    SafeRelease(client);
    SafeRelease(device);
    SafeRelease(enumerator);
    return g_mic_serial.load() != serial ? Outcome::Reopen : outcome;
}

// The same through the test seam's fake devices (same selection, same chain).
Outcome FakeCaptureSession(const Worker &w, testing::FakeDevices *fake) {
    Session &s = *w.s;
    const uint32_t serial = g_mic_serial.load();
    int default_index = -1;
    const std::vector<devices::Candidate> list = fake->CaptureList(default_index);
    const std::wstring chosen = PickFrom(list, default_index);
    dsp::DeviceFormat format;
    if (chosen.empty() || !fake->OpenCapture(chosen, format)) {
        if (chosen.empty()) Log("voice: no microphone (no input, or only monitor/loopback inputs); listening only, checking every 2 s");
        MicState(w, false);
        return chosen.empty() ? Outcome::Retry : Outcome::RetrySlow;
    }
    MicState(w, true);
    Log("voice: microphone open (%s)", FormatText(format).c_str());
    RestartCapture(w);
    std::vector<uint8_t> buffer;
    Outcome outcome = Outcome::Retry;
    for (;;) {
        if (WaitIdle(w, 1, &s.stop, 5) == WAIT_OBJECT_0 || !Live(w)) break;
        const int frames = fake->ReadCapture(buffer);
        if (frames < 0) { Log("voice: microphone lost or its format changed; reopening"); outcome = Outcome::Reopen; break; }
        if (frames > 0 && !Feed(w, format, buffer.data(), static_cast<size_t>(frames), false)) break;
        if (g_mic_serial.load() != serial) { outcome = Outcome::Reopen; break; }
    }
    fake->CloseCapture();
    return outcome;
}

// The mix (48 kHz stereo) in the output's own format: rate, channels (L → 0, R → 1, the rest silent; one channel:
// (L + R) / 2) and sample type.
class RenderConverter {
public:
    void Reset(const dsp::DeviceFormat &f) {
        f_ = f;
        left_.Reset(kRate, f.rate);
        right_.Reset(kRate, f.rate);
        fifo_.clear();
    }
    // `pull` gives 480 frames of the mix (false: stop). Writes `frames` device frames to `out`.
    template <typename Pull> bool Fill(uint8_t *out, size_t frames, Pull &&pull) {
        while (fifo_.size() / 2 < frames) {
            float stereo[2 * kSub], l[kSub], r[kSub];
            if (!pull(stereo, static_cast<size_t>(kSub))) return false;
            for (int i = 0; i < kSub; i++) { l[i] = stereo[2 * i]; r[i] = stereo[2 * i + 1]; }
            left_.Process(l, kSub, lo_);
            right_.Process(r, kSub, ro_);
            const size_t n = std::min(lo_.size(), ro_.size());
            for (size_t i = 0; i < n; i++) { fifo_.push_back(lo_[i]); fifo_.push_back(ro_[i]); }
        }
        const size_t stride = static_cast<size_t>(f_.container) * f_.channels;
        for (size_t i = 0; i < frames; i++) {
            const float l = fifo_[2 * i], r = fifo_[2 * i + 1];
            uint8_t *p = out + i * stride;
            for (uint16_t c = 0; c < f_.channels; c++)
                dsp::WriteSample(f_, p + static_cast<size_t>(c) * f_.container, f_.channels == 1 ? (l + r) * 0.5f : c == 0 ? l : c == 1 ? r : 0.0f);
        }
        fifo_.erase(fifo_.begin(), fifo_.begin() + static_cast<std::ptrdiff_t>(2 * frames));
        return true;
    }

private:
    dsp::DeviceFormat f_;
    dsp::Resampler left_, right_;
    std::vector<float> lo_, ro_, fifo_;
};

bool PullFromMixer(const Worker &w, float *stereo, size_t frames) {
    std::lock_guard lock(w.s->mix_lock);
    if (!Live(w)) return false;
    w.s->mixer->Pull(stereo, frames);
    return true;
}

void OutputState(const Worker &w, bool ok, const std::wstring &in_use) {
    if (!Live(w)) return;
    FirstTry(*w.s, 1, ok);
    SetOutputInUse(in_use); // §15: the device the mix (and so the echo reference) goes to
}

// One open playback session (WASAPI shared render).
Outcome PlaybackSession(const Worker &w, HANDLE event) {
    Session &s = *w.s;
    const uint32_t serial = g_output_serial.load();
    IMMDeviceEnumerator *enumerator = nullptr;
    IMMDevice *device = nullptr;
    IAudioClient *client = nullptr;
    IAudioRenderClient *render = nullptr;
    WAVEFORMATEX *mix = nullptr;
    dsp::DeviceFormat format;
    UINT32 buffer_frames = 0;
    HRESULT hr = CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL, IID_PPV_ARGS(&enumerator));
    const std::wstring output = SUCCEEDED(hr) ? DefaultRenderId(enumerator) : std::wstring();
    // §15: the player's output while it is there (empty: the default, followed by the poll below)
    std::wstring chosen = SUCCEEDED(hr) ? SelectOutput(enumerator) : std::wstring();
    thread_local bool refused = false;   // the chosen output would not open: the default until the player picks again
    thread_local uint32_t refused_serial = 0;
    if (refused && refused_serial != serial) refused = false;
    if (refused) chosen.clear();
    refused_serial = serial;
    if (!OutputChoice().empty() && chosen.empty() && !refused) {
        thread_local std::wstring missing_logged;
        if (missing_logged != OutputChoice()) { missing_logged = OutputChoice(); Log("voice: the chosen output is not there; using the system default"); }
    }
    const std::wstring target = chosen.empty() ? output : chosen;
    if (SUCCEEDED(hr) && target.empty()) hr = HRESULT_FROM_WIN32(ERROR_NOT_FOUND);
    if (SUCCEEDED(hr)) hr = enumerator->GetDevice(target.c_str(), &device);
    if (SUCCEEDED(hr)) hr = device->Activate(__uuidof(IAudioClient), CLSCTX_ALL, nullptr, reinterpret_cast<void **>(&client));
    if (SUCCEEDED(hr)) hr = client->GetMixFormat(&mix);
    if (SUCCEEDED(hr) && !ParseFormat(mix, format)) hr = E_NOTIMPL;
    if (SUCCEEDED(hr)) hr = client->Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 1000000, 0, mix, nullptr);
    if (SUCCEEDED(hr)) hr = client->SetEventHandle(event);
    if (SUCCEEDED(hr)) hr = client->GetBufferSize(&buffer_frames);
    if (SUCCEEDED(hr)) hr = client->GetService(IID_PPV_ARGS(&render));
    thread_local HRESULT last_error = S_OK;
    Outcome outcome = Outcome::Retry;
    if (FAILED(hr) && !chosen.empty()) {
        Log("voice: the chosen output refused (0x%08lx); using the system default", static_cast<unsigned long>(hr));
        refused = true; // until the player picks again: no reopen loop on a device that will not open
        outcome = Outcome::Reopen;
    } else if (FAILED(hr)) {
        if (hr != last_error) Log("voice: no audio output (0x%08lx); retrying", static_cast<unsigned long>(hr));
        last_error = hr;
        OutputState(w, false, std::wstring());
        outcome = hr == HRESULT_FROM_WIN32(ERROR_NOT_FOUND) ? Outcome::Retry : Outcome::RetrySlow;
    } else {
        last_error = S_OK;
        OutputState(w, true, target);
        Log("voice: playback open (%s)", FormatText(format).c_str());
        RenderConverter converter;
        converter.Reset(format);
        const UINT32 queued = std::min<UINT32>(buffer_frames, format.rate * kQueuedMs / 1000);
        bool started = false;
        ULONGLONG last_check = GetTickCount64();
        for (;;) {
            UINT32 padding = 0;
            if (FAILED(hr = client->GetCurrentPadding(&padding))) break;
            if (padding < queued) {
                const UINT32 want = queued - padding;
                BYTE *data = nullptr;
                if (FAILED(hr = render->GetBuffer(want, &data))) break;
                const bool ok = converter.Fill(data, want, [&](float *stereo, size_t frames) { return PullFromMixer(w, stereo, frames); });
                render->ReleaseBuffer(want, ok ? 0 : AUDCLNT_BUFFERFLAGS_SILENT);
                if (!ok) break;
            }
            if (!started) { if (FAILED(hr = client->Start())) break; started = true; }
            HANDLE waits[2] = {s.stop, event};
            if (WaitIdle(w, 2, waits, 20) == WAIT_OBJECT_0 || !Live(w)) break;
            if (g_output_serial.load() != serial) { Log("voice: the player chose another output; reopening"); outcome = Outcome::Reopen; break; } // §15
            if (GetTickCount64() - last_check > 3000) {
                last_check = GetTickCount64();
                // the chosen output came back or went away (§15), or (on the default) the default changed
                if (!refused && SelectOutput(enumerator) != chosen) { Log("voice: the chosen output came or went; reopening"); outcome = Outcome::Reopen; break; }
                if (chosen.empty() && DefaultRenderId(enumerator) != output) { Log("voice: default audio output changed; reopening"); outcome = Outcome::Reopen; break; }
            }
        }
        if (hr == AUDCLNT_E_DEVICE_INVALIDATED) { Log("voice: audio output lost or its format changed; reopening"); outcome = Outcome::Reopen; }
        else if (FAILED(hr)) Log("voice: audio output lost (0x%08lx); retrying", static_cast<unsigned long>(hr));
        if (started) client->Stop();
        if (Live(w)) SetOutputInUse(std::wstring());
    }
    if (mix) CoTaskMemFree(mix);
    SafeRelease(render);
    SafeRelease(client);
    SafeRelease(device);
    SafeRelease(enumerator);
    return g_output_serial.load() != serial ? Outcome::Reopen : outcome;
}

Outcome FakePlaybackSession(const Worker &w, testing::FakeDevices *fake) {
    Session &s = *w.s;
    const uint32_t serial = g_output_serial.load();
    if (!fake->OpenRender()) { OutputState(w, false, std::wstring()); return Outcome::RetrySlow; }
    OutputState(w, true, L"fake");
    Log("voice: playback open (fake output, 48000 Hz, 2 channel(s))");
    float stereo[2 * kSub];
    Outcome outcome = Outcome::Retry;
    for (;;) {
        if (WaitIdle(w, 1, &s.stop, 5) == WAIT_OBJECT_0 || !Live(w)) break;
        if (!PullFromMixer(w, stereo, kSub)) break;
        if (!fake->Render(stereo, kSub)) { Log("voice: audio output lost; retrying"); break; }
        if (g_output_serial.load() != serial) { Log("voice: the player chose another output; reopening"); outcome = Outcome::Reopen; break; }
    }
    if (Live(w)) SetOutputInUse(std::wstring());
    fake->CloseRender();
    return outcome;
}

// ---------------------------------------------------------------- echo reference: loopback of the output in use

// Release 2.0.0: the echo canceller's far end is what the speakers play. The voice mix alone misses the game (cars,
// music), so while it works the reference is a WASAPI loopback of the very endpoint the voice plays to (OutputInUse:
// the player's chosen output, or the default while it stands in), which carries the game and the voice mix together.
// It follows that endpoint (reopened within 0.5 s of a change). Without it (no output open, loopback refused, as an
// older Wine may: inference, not yet run on a Deck) the mix is the reference, as in the headless tests. EchoLink picks
// one of the two at a time; a switch costs AEC3 a re-alignment (about a second).
Outcome LoopbackSession(const Worker &w) {
    Session &s = *w.s;
    const std::wstring target = OutputInUse();
    if (target.empty()) return Outcome::Retry; // no output open: nothing plays, the retry waits
    IMMDeviceEnumerator *enumerator = nullptr;
    IMMDevice *device = nullptr;
    IAudioClient *client = nullptr;
    IAudioCaptureClient *capture = nullptr;
    WAVEFORMATEX *mix = nullptr;
    dsp::DeviceFormat format;
    HRESULT hr = CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL, IID_PPV_ARGS(&enumerator));
    if (SUCCEEDED(hr)) hr = enumerator->GetDevice(target.c_str(), &device);
    if (SUCCEEDED(hr)) hr = device->Activate(__uuidof(IAudioClient), CLSCTX_ALL, nullptr, reinterpret_cast<void **>(&client));
    if (SUCCEEDED(hr)) hr = client->GetMixFormat(&mix);
    if (SUCCEEDED(hr) && !ParseFormat(mix, format)) hr = E_NOTIMPL;
    // polled every 10 ms: loopback + event callback is not dependable on older Windows nor on Wine (inference)
    if (SUCCEEDED(hr)) hr = client->Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 200000, 0, mix, nullptr);
    if (SUCCEEDED(hr)) hr = client->GetService(IID_PPV_ARGS(&capture));
    if (SUCCEEDED(hr)) hr = client->Start();
    thread_local HRESULT last_error = S_OK;
    Outcome outcome = Outcome::Retry;
    if (FAILED(hr)) {
        if (hr != last_error)
            Log("voice: no loopback of the output (0x%08lx); the echo canceller hears the voice mix only", static_cast<unsigned long>(hr));
        last_error = hr;
        outcome = Outcome::RetrySlow;
    } else {
        last_error = S_OK;
        Log("voice: output loopback open (%s); the echo reference once it delivers steadily", FormatText(format).c_str());
        dsp::Resampler resampler;
        resampler.Reset(format.rate);
        std::vector<float> mono, at48;
        float block[echo::Canceller::kBlock];
        int fill = 0;
        ULONGLONG last_check = GetTickCount64();
        if (Live(w)) s.echo.loopback_open = true;
        bool done = false;
        while (!done) {
            if (WaitIdle(w, 1, &s.stop, 10) == WAIT_OBJECT_0 || !Live(w)) break;
            UINT32 packet = 0;
            while (SUCCEEDED(hr = capture->GetNextPacketSize(&packet)) && packet) {
                BYTE *data = nullptr;
                UINT32 frames = 0;
                DWORD flags = 0;
                hr = capture->GetBuffer(&data, &frames, &flags, nullptr, nullptr);
                if (FAILED(hr)) break;
                if ((flags & AUDCLNT_BUFFERFLAGS_SILENT) || !data) mono.assign(frames, 0.0f);
                else dsp::ToMono(format, data, frames, mono);
                capture->ReleaseBuffer(frames);
                if (!Live(w)) { done = true; break; }
                resampler.Process(mono.data(), mono.size(), at48);
                for (const float x : at48) {
                    block[fill++] = std::clamp(x, -1.0f, 1.0f);
                    if (fill == echo::Canceller::kBlock) { fill = 0; s.echo.LoopbackBlock(block); }
                }
            }
            if (done) break;
            if (FAILED(hr)) { Log("voice: output loopback lost (0x%08lx); retrying", static_cast<unsigned long>(hr)); break; }
            if (GetTickCount64() - last_check > 500) {
                last_check = GetTickCount64();
                if (OutputInUse() != target) { outcome = Outcome::Reopen; break; } // voice moved to another output: follow it
            }
        }
        if (Current(w)) s.echo.loopback_open = false;
        client->Stop();
    }
    if (mix) CoTaskMemFree(mix);
    SafeRelease(capture);
    SafeRelease(client);
    SafeRelease(device);
    SafeRelease(enumerator);
    return outcome;
}

// ---------------------------------------------------------------- the device workers and their watchdog

DWORD WINAPI RoleThread(void *arg);
DWORD WINAPI WatchdogThread(void *arg);

bool SpawnWorker(const std::shared_ptr<Session> &s, int role, uint32_t gen, DWORD start_delay_ms) {
    auto *w = new (std::nothrow) Worker{s, role, gen, start_delay_ms};
    if (!w) return false;
    g_workers_alive++; // counted from here: a worker that never starts is uncounted below
    HANDLE t = CreateThread(nullptr, 0, role == kWatchdogRole ? WatchdogThread : RoleThread, w, 0, nullptr);
    if (!t) { g_workers_alive--; delete w; return false; }
    std::lock_guard lock(s->threads_lock);
    s->threads.push_back(t);
    return true;
}

Outcome RunSession(const Worker &w, HANDLE event) {
    testing::FakeDevices *fake = g_fake.load();
    switch (w.role) {
    case kCaptureRole: return fake ? FakeCaptureSession(w, fake) : CaptureSession(w, event);
    case kPlaybackRole: return fake ? FakePlaybackSession(w, fake) : PlaybackSession(w, event);
    default: return fake ? Outcome::Retry : LoopbackSession(w);
    }
}

DWORD WINAPI RoleThread(void *arg) {
    const std::unique_ptr<Worker> w(static_cast<Worker *>(arg));
    Session &s = *w->s;
    FlushDenormals();
    Busy(*w);
    const HRESULT com = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    HANDLE event = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    try {
        if (w->start_delay_ms && WaitIdle(*w, 1, &s.stop, w->start_delay_ms) == WAIT_OBJECT_0) {
            // stopped before the retry
        } else if (FAILED(com) || !event) {
            Log("voice: the %s worker has no COM/event", kRoleNames[w->role]);
        } else {
            int slow = 0;
            while (Live(*w)) {
                const Outcome outcome = RunSession(*w, event);
                if (!Live(*w)) break;
                DWORD wait = w->role == kLoopbackRole ? 500 : kRetryMs;
                if (outcome == Outcome::Reopen) wait = 0;
                if (outcome == Outcome::RetrySlow) wait = std::min<DWORD>(16000, kRetryMs << std::min(slow++, 3));
                else slow = 0;
                if (WaitIdle(*w, 1, &s.stop, wait) == WAIT_OBJECT_0) break;
            }
        }
    } catch (...) {
        if (Live(*w)) {
            if (w->role == kLoopbackRole) Log("voice: the output loopback failed; the echo canceller hears the voice mix only"); // voice goes on
            else Fail(w->role == kCaptureRole ? "the capture thread failed" : "the playback thread failed");
        }
    }
    Idle(*w);
    if (Current(*w)) {
        if (w->role < 2) FirstTry(s, w->role, false); // only if it never tried: Start stops waiting
        if (w->role == kLoopbackRole) s.echo.loopback_open = false;
    }
    if (event) CloseHandle(event);
    if (SUCCEEDED(com)) CoUninitialize();
    g_workers_alive--;
    return 0; // the session goes with the last reference (this one, for a worker left behind)
}

// A worker whose device call has not returned for g_stuck_ms is left behind; a fresh one retries after a backoff. At
// most kMaxAbandon per role, and never past kMaxWorkers alive: then that part of voice stays off (the game goes on).
void Abandon(const std::shared_ptr<Session> &s, int role, int64_t stuck_ms) {
    RoleState &r = s->roles[role];
    const uint32_t gen = ++r.gen;
    r.busy_since = 0;
    const int n = ++r.abandoned;
    Log("voice: the %s worker's device call has not returned for %lld ms; it is left behind, a fresh one retries", kRoleNames[role],
        static_cast<long long>(stuck_ms));
    if (role == kCaptureRole) g_mic_ok = false;
    if (role == kPlaybackRole) SetOutputInUse(std::wstring());
    if (role == kLoopbackRole) s->echo.loopback_open = false;
    if (n > kMaxAbandon || g_workers_alive.load() >= kMaxWorkers || !SpawnWorker(s, role, gen, kRetryMs * static_cast<DWORD>(n))) {
        r.active = false;
        Log("voice: the %s does not answer; that part of voice stays off for this session", kRoleNames[role]);
    }
}

DWORD WINAPI WatchdogThread(void *arg) {
    const std::unique_ptr<Worker> w(static_cast<Worker *>(arg));
    while (WaitForSingleObject(w->s->stop, 100) == WAIT_TIMEOUT && Live(*w))
        for (int role = 0; role < kRoles; role++) {
            RoleState &r = w->s->roles[role];
            const int64_t since = r.busy_since.load();
            if (r.active.load() && since && RealMs() - since > g_stuck_ms.load()) Abandon(w->s, role, RealMs() - since);
        }
    g_workers_alive--;
    return 0;
}

// Ends a session: its workers are told to stop and waited for at most `wait_ms` (on the voice thread, never the
// game's); any still in a device call after that are left behind with their own reference.
void Retire(const std::shared_ptr<Session> &s, DWORD wait_ms) {
    s->running = false;
    if (s->stop) SetEvent(s->stop);
    std::vector<HANDLE> threads;
    {
        std::lock_guard lock(s->threads_lock);
        threads = s->threads;
    }
    if (threads.empty() || !wait_ms) return;
    if (threads.size() > MAXIMUM_WAIT_OBJECTS) threads.resize(MAXIMUM_WAIT_OBJECTS);
    if (WaitForMultipleObjects(static_cast<DWORD>(threads.size()), threads.data(), TRUE, wait_ms) == WAIT_TIMEOUT)
        Log("voice: an audio device call has not returned; its worker is left behind (voice stops, the game goes on)");
}

std::shared_ptr<Session> TakeSession() {
    std::unique_lock lock(g_life);
    g_running = false;
    // no permission carries over to the next session: its capture worker sends nothing until VoiceThread's SetTransmit
    g_allowed = false;
    g_ptt = false;
    g_sending_until = 0;
    SetOutputInUse(std::wstring()); // its playback worker, no longer live, does not clear it
    return std::move(g_session);
}

void Publish(const std::shared_ptr<Session> &s) {
    std::unique_lock lock(g_life);
    g_session = s;
    g_running = true;
}

std::shared_ptr<Session> CreateSession(SendFn send) {
    if (g_failed.load()) return nullptr;
    try {
        auto s = std::make_shared<Session>();
        s->echo.canceller = std::make_unique<echo::Canceller>(); // always on (OD-144): voice does not start without it
        s->capture = std::make_unique<CaptureChain>(std::move(send), &s->echo);
        s->mixer = std::make_unique<Mixer>(&s->echo);
        if (!s->capture->Init() || !s->mixer->Init()) { Fail("a codec library failed to start"); return nullptr; }
        if (!s->echo.canceller->Init()) { Fail("the echo canceller failed to start"); return nullptr; }
        s->stop = CreateEventW(nullptr, TRUE, FALSE, nullptr);
        if (!s->stop) { Fail("no stop event"); return nullptr; }
        g_concealed = 0;
        return s;
    } catch (...) {
        Fail("the engine could not be created");
        return nullptr;
    }
}

// The session, under a shared lock taken with a try: null when Start/Stop holds the pointer this instant (the caller
// drops that one packet or answers "not talking" for one frame). Never waits.
#define FI_TRY_SESSION(lock, s)                                \
    std::shared_lock lock(g_life, std::try_to_lock);           \
    Session *s = lock.owns_lock() ? g_session.get() : nullptr

} // namespace

bool Start(SendFn send) {
    try {
        if (g_running.load()) return true;
        if (const auto old = TakeSession()) Retire(old, 0); // a session that failed on its own
        FlushDenormals();
        if (g_failed.load()) return false;
        if (g_workers_alive.load() > kMaxWorkers - 4) {
            static std::atomic<bool> said{false};
            if (!said.exchange(true)) Log("voice: earlier audio device calls never returned; voice stays off for this game session");
            return false;
        }
        const std::shared_ptr<Session> s = CreateSession(std::move(send));
        if (!s) return false;
        for (int i = 0; i < 2; i++) s->first_try[i] = CreateEventW(nullptr, TRUE, FALSE, nullptr);
        g_mic_ok = false;
        s->roles[kCaptureRole].active = s->roles[kPlaybackRole].active = true;
        // the watchdog first: a first device call that hangs is caught like any later one
        if (!s->first_try[0] || !s->first_try[1] || !SpawnWorker(s, kWatchdogRole, 0, 0) || !SpawnWorker(s, kCaptureRole, 0, 0) ||
            !SpawnWorker(s, kPlaybackRole, 0, 0)) {
            Log("voice: engine threads unavailable");
            Retire(s, 500);
            return false;
        }
        // the first device attempts, waited for here on the voice thread (no lock held: the game never waits on it)
        WaitForMultipleObjects(2, s->first_try, TRUE, 3000);
        const bool mic = s->device_ok[0].load() == 1, out = s->device_ok[1].load() == 1;
        if (!mic && !out) {
            Log("voice: no microphone and no audio output; voice stays off");
            Retire(s, 500);
            return false;
        }
        if (!mic) Log("voice: listening only until a microphone appears");
        if (!out) Log("voice: talking only until an audio output appears");
        if (!g_fake.load()) {
            s->roles[kLoopbackRole].active = true; // 2.0.0: optional, voice goes on without
            if (!SpawnWorker(s, kLoopbackRole, 0, 0)) { s->roles[kLoopbackRole].active = false; Log("voice: no loopback thread; the echo canceller hears the voice mix only"); }
        }
        Publish(s);
        Log("voice: audio engine started (decoder complexity %d)", g_complexity.load());
        return true;
    } catch (...) {
        Log("voice: Start failed");
        return false;
    }
}

void Stop() {
    try {
        if (const auto old = TakeSession()) Retire(old, 1500);
    } catch (...) {
        Log("voice: Stop failed");
    }
}

bool Running() { return g_running.load(); }

bool ListenOnly() { return g_running.load() && !g_mic_ok.load(); }

void SetTransmit(bool allowed, bool push_to_talk) {
    g_ptt = push_to_talk;
    g_allowed = allowed;
    if (!allowed) g_sending_until = 0;
}

void SetBundle(int frames) { g_bundle = frames == 6 ? 6 : 3; }

void Receive(uint8_t seat, uint16_t seq, uint32_t ts, uint8_t flags, const uint8_t *opus, size_t size) {
    try {
        FI_TRY_SESSION(lock, s);
        if (s && g_running.load()) s->mixer->Receive(seat, seq, ts, flags, opus, size);
    } catch (...) {
        Fail("Receive failed");
    }
}

void SetSilenced(uint8_t seat, bool silenced) {
    std::shared_lock lock(g_life); // the voice thread; the exclusive side is only ever a pointer swap
    if (g_session) g_session->mixer->SetSilenced(seat, silenced);
}

void SetVolume(float voices) {
    std::shared_lock lock(g_life);
    if (g_session) g_session->mixer->SetVolume(voices);
}

void SetMicGain(float gain) { g_mic_gain.store(SafeGain(gain), std::memory_order_relaxed); }

void SetSeatGain(uint8_t seat, float gain) {
    if (seat < kSeats) g_seat_gains.gain[seat].store(SafeGain(gain), std::memory_order_relaxed);
}

namespace {
std::wstring Wide(const std::string &s) {
    if (s.empty()) return {};
    const int n = MultiByteToWideChar(CP_UTF8, 0, s.data(), static_cast<int>(s.size()), nullptr, 0);
    std::wstring w(n > 0 ? n : 0, L'\0');
    if (n > 0) MultiByteToWideChar(CP_UTF8, 0, s.data(), static_cast<int>(s.size()), w.data(), n);
    return w;
}
} // namespace

void SetDevices(const std::string &microphone_id, const std::string &output_id) {
    const std::wstring mic = Wide(microphone_id), out = Wide(output_id);
    std::lock_guard lock(g_choice_lock);
    if (mic != g_mic_choice) { g_mic_choice = mic; g_mic_serial++; }
    if (out != g_output_choice) { g_output_choice = out; g_output_serial++; }
}

std::string OutputDeviceInUse() { return Utf8(OutputInUse()); }

std::vector<Device> ListDevices(bool capture) {
    std::vector<Device> out;
    try {
        const HRESULT com = CoInitializeEx(nullptr, COINIT_MULTITHREADED); // RPC_E_CHANGED_MODE: COM is there already
        IMMDeviceEnumerator *enumerator = nullptr;
        if (SUCCEEDED(CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL, IID_PPV_ARGS(&enumerator)))) {
            int default_index = -1;
            const std::vector<devices::Candidate> list = capture ? CaptureCandidates(enumerator, default_index, nullptr) : RenderCandidates(enumerator);
            for (const devices::Choice &c : devices::Choices(list, capture)) out.push_back({Utf8(c.id), Utf8(c.name)});
        }
        SafeRelease(enumerator);
        if (SUCCEEDED(com)) CoUninitialize();
    } catch (...) {
        out.clear();
    }
    if (out.empty()) out.push_back({}); // "System default" is always there
    return out;
}

bool SeatTalking(uint8_t seat) {
    FI_TRY_SESSION(lock, s);
    return s && g_running.load() && s->mixer->Talking(seat);
}

bool SeatHeard(uint8_t seat) {
    FI_TRY_SESSION(lock, s);
    return s && g_running.load() && seat < kSeats && s->mixer->Heard(seat);
}

void SetVoiceMode(VoiceMode mode) {
    const auto m = static_cast<uint8_t>(mode);
    g_voice_mode.store(m <= static_cast<uint8_t>(spatial::Mode::Proximity) ? m : static_cast<uint8_t>(spatial::Mode::Open));
}

void SetPose(uint8_t seat, const spatial::Pose &talker, const spatial::Pose &self) {
    try {
        FI_TRY_SESSION(lock, s);
        if (s && g_running.load()) s->mixer->SetPose(seat, talker, self);
    } catch (...) {
        Fail("SetPose failed");
    }
}

void SetSpatialParams(const spatial::Params &params) {
    {
        std::lock_guard lock(g_params_lock);
        g_params = spatial::Sanitized(params);
    }
    g_params_serial++;
}

bool Sending() { return g_running.load() && g_allowed.load() && NowMs() < g_sending_until.load(); }

namespace testing {

bool StartHeadless(SendFn send) {
    Stop();
    g_failed = false; // a test session starts clean
    g_mic_ok = true;  // the test feeds capture itself
    const std::shared_ptr<Session> s = CreateSession(std::move(send));
    if (!s) return false;
    Publish(s);
    return true;
}

void ProcessCaptureFormat(const dsp::DeviceFormat &format, const uint8_t *data, size_t frames) {
    std::shared_lock lock(g_life);
    if (!g_running.load() || !g_session) return;
    FlushDenormals();
    try {
        std::lock_guard feed(g_session->capture_lock);
        g_session->capture->FeedDevice(format, data, frames, false);
    } catch (...) {
        Fail("capture processing failed");
    }
}

void ProcessCapture(const float *interleaved, size_t frames, uint32_t rate, uint16_t channels) {
    ProcessCaptureFormat(dsp::DeviceFormat{rate, channels, dsp::SampleKind::Float32, 4}, reinterpret_cast<const uint8_t *>(interleaved), frames);
}

size_t PullMixStereo(float *interleaved, size_t frames) {
    std::shared_lock lock(g_life);
    if (!g_running.load() || !g_session) { std::fill(interleaved, interleaved + 2 * frames, 0.0f); return 0; }
    FlushDenormals();
    try {
        std::lock_guard pull(g_session->mix_lock);
        return g_session->mixer->Pull(interleaved, frames);
    } catch (...) {
        Fail("mixing failed");
        return 0;
    }
}

size_t PullMix(float *out, size_t samples) {
    std::vector<float> stereo(2 * samples);
    const size_t got = PullMixStereo(stereo.data(), samples);
    for (size_t i = 0; i < samples; i++) out[i] = (stereo[2 * i] + stereo[2 * i + 1]) * 0.5f; // open mode: exactly the mono mix
    return got;
}

void SetClockMs(int64_t ms) { g_fake_now = ms; }
int DecoderComplexity() { return g_complexity.load(); }
void SetDecodeCostOverrideUs(double us) { g_cost_override = us; }
double DecodeCostP95Us() { return g_cost_p95.load(); }
uint64_t ConcealedFrames() { return g_concealed.load(); }
void SetEchoTap(EchoTap tap) { g_echo_tap.store(tap); }
void SetMixReference(bool on) { g_mix_reference.store(on); }
void SetFakeDevices(FakeDevices *devices) { g_fake.store(devices); }
void SetStuckMs(int ms) { g_stuck_ms.store(ms > 0 ? ms : 8000); }
int WorkersAlive() { return g_workers_alive.load(); }

} // namespace testing
} // namespace fi::audio
