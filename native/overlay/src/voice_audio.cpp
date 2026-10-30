// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: voice chat's audio engine (OD-154, VC-3; VOICE_CHAT.md §3.2, §12.4). Two engine threads, never
// the game's: capture (WASAPI shared, event-driven, the default communications microphone) → 48 kHz mono → 80 Hz
// high-pass → AGC → RNNoise (noise suppression; its voice probability gates sending, 300 ms hangover) → Opus 20 ms
// frames (SILK wideband 6 kbit/s, CVBR, complexity 10, no FEC/DTX) → repacketized 3 (or 6) per packet → SendFn; and
// playback: per seat an adaptive 1-2 packet jitter buffer → Opus decoder with deep PLC for gaps → mix → one XAudio2
// source voice (3 × 20 ms queued, ~60 ms). The decoder starts at complexity 7 (NoLACE) on every platform and steps
// down only on measured decode cost (voice_audio_complexity.h): no platform-based quality choice, so a Deck player
// gets what a Windows player gets. Releasing transmit (push-to-talk, mute) ends the spurt with a short packet that
// carries the end flag. Every failure logs and turns voice off; a missing, unplugged or changed device is retried or
// reopened every 2-3 s. Nothing here touches the network.
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
// | GetMixFormat → WAVEFORMATEXTENSIBLE float32    | usual   | known: winepulse reports extensible float32 at the PulseAudio/PipeWire rate (44.1 or 48 kHz), N channels | any PCM16/24/32 or float32, any rate/channels, converted by voice_audio_dsp.h |
// | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM / SRC_DEFAULT_QUALITY | Win10+ | newer Wine only; quality differs (inference) | **not used**: our own downmix + windowed-sinc resampler on both |
// | Shared mode + AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 20 ms buffer | yes | known; Wine rounds the period to its own (~10 ms) | packet sizes are not assumed: every wake drains all packets; 500 ms wait timeout as a fallback |
// | AUDCLNT_E_DEVICE_INVALIDATED on unplug         | yes     | not guaranteed: PulseAudio moves the stream (inference) | also: no data for 2 s = reopen; default device id polled every 3 s (capture and render) |
// | IMMNotificationClient                          | yes     | partial (inference)                           | **not used**: polling works on both                    |
// | MMCSS (AvSetMmThreadCharacteristics, avrt.dll) | yes     | stub / may be absent                          | **not used** (no avrt import); plain threads          |
// | FTZ/DAZ via _mm_setcsr on each engine thread   | yes     | known: MXCSR is per thread CPU state, Wine keeps it | set on capture, playback and test-seam entry       |
// | xaudio2_9.dll via LoadLibraryExW(SYSTEM32)     | yes     | known: Wine/Proton builtin, FAudio-backed     | loaded at run time (no import), as toast_audio.cpp     |
// | XAudio2Create(0, XAUDIO2_DEFAULT_PROCESSOR)    | yes     | known (FAudio)                                | flags 0 only                                           |
// | Mastering voice on the default device; float32 mono 48 kHz source voice; OnBufferEnd | yes | known (FAudio) | used; also a 100 ms wait timeout so a missed callback never starves the queue |
// | GetState(XAUDIO2_VOICE_NOSAMPLESPLAYED)        | yes     | known (FAudio)                                | used                                                   |
// | OnCriticalError on device loss                 | yes     | may not fire (inference)                      | plus the render default-device poll                    |
// | Processor affinity / priority, GetActiveProcessorCount, IsProcessorFeaturePresent, wine_get_version | yes | yes | **not used**: no platform input anywhere |
// | QueryPerformanceCounter, GetTickCount64, CreateThread, events | yes | known                                  | used (decode-cost timing, clocks, threads)             |
// | libopus RTCD (cpuid: SSE4.1/AVX2)              | yes     | known: cpuid is the real CPU (Deck: Zen 2, AVX2) | same code paths on both CPUs with AVX2              |
#include "voice_audio.h"
#include "voice_audio_complexity.h"
#include "voice_audio_dsp.h"
#include "voice_audio_testing.h"

#include "overlay.h"

#include <mmreg.h>
#include <audioclient.h>
#include <mmdeviceapi.h>
#include <xaudio2.h>
#include <xmmintrin.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <cmath>
#include <deque>
#include <memory>
#include <mutex>
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
constexpr int kQueuedBuffers = 3;      // 3 × 20 ms in XAudio2: the ~60 ms target
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
HANDLE g_stop = nullptr;

void Fail(const char *what) {
    if (!g_failed.exchange(true)) Log("voice: %s; voice chat is off for this session", what);
    g_running = false;
    if (g_stop) SetEvent(g_stop);
}

// ---------------------------------------------------------------- capture chain

class CaptureChain {
public:
    explicit CaptureChain(SendFn send) : send_(std::move(send)) {}
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
            Log("voice: capture %u Hz, %u channel(s), %u-byte samples", format.rate, format.channels, format.container);
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
            sub_[fill_++] = hp_.Process(x[i]) * 32768.0f;
            if (fill_ == kSub) { fill_ = 0; Block(); }
        }
    }
    void Block() {
        // transmit released (push-to-talk let go, muted, voice off) during a spurt: end it now, not at the bundle's end
        if (prev_sent_ && !g_allowed.load()) EndSpurt();
        if (subs_ == 0) bundle_ = std::clamp(g_bundle.load(), 3, kMaxBundle) == 6 ? 6 : 3; // a packet's size is fixed at its start
        agc_.Process(sub_, kSub, prev_voice_, kVoiceThreshold);
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
    DenoiseState *ns_ = nullptr;
    OpusEncoder *enc_ = nullptr;
    OpusRepacketizer *rp_ = nullptr;
    dsp::DeviceFormat format_{0, 0, dsp::SampleKind::Float32, 0};
    dsp::Resampler resampler_;
    dsp::HighPass hp_;
    dsp::Agc agc_;
    std::vector<float> mono_, at48_;
    float sub_[kSub] = {};
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
    Mixer() = default;
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
        if (seat >= kSeats || !opus || size < 1 || size > 400 || silenced_[seat].load()) return;
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
    void SetVolume(float v) { volume_ = std::clamp(std::isfinite(v) ? v : 0.0f, 0.0f, 1.0f); }
    size_t Pull(float *out, size_t n) {
        size_t done = 0;
        while (done < n) {
            if (mix_pos_ == kFrame) { MixFrame(mix_); mix_pos_ = 0; }
            const size_t take = std::min(n - done, static_cast<size_t>(kFrame - mix_pos_));
            std::memcpy(out + done, mix_ + mix_pos_, take * sizeof(float));
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
    void MixFrame(float *out) {
        DrainInbox();
        std::fill(out, out + kFrame, 0.0f);
        const float volume = volume_.load();
        decodes_ = 0;
        int talkers = 0;
        const double started = QpcUs();
        for (int i = 0; i < kSeats; i++) {
            Seat &s = seats_[i];
            if (silenced_[i].load()) { if (s.playing || !s.buffer.empty() || s.pcm_len) s.Clear(); continue; }
            int filled = 0;
            const int decodes_before = decodes_;
            while (filled < kFrame) {
                if (s.pcm_pos >= s.pcm_len && !Refill(s)) break;
                const int take = std::min(kFrame - filled, s.pcm_len - s.pcm_pos);
                for (int k = 0; k < take; k++) out[filled + k] += s.pcm[s.pcm_pos + k] * volume;
                s.pcm_pos += take;
                filled += take;
            }
            if (filled || decodes_ != decodes_before) talkers++;
        }
        const double cost = QpcUs() - started;
        for (int k = 0; k < kFrame; k++) out[k] = std::clamp(out[k], -1.0f, 1.0f);
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

    ComplexityController control_;
    int decodes_ = 0; // 20 ms frames decoded or concealed in this mix frame
    std::array<Seat, kSeats> seats_;
    std::array<std::atomic<bool>, kSeats> silenced_{};
    std::array<std::atomic<int64_t>, kSeats> last_rx_{};
    std::atomic<float> volume_{1.0f};
    std::mutex inbox_mutex_;
    std::vector<Packet> inbox_;
    float mix_[kFrame] = {};
    int mix_pos_ = kFrame;
};

// ---------------------------------------------------------------- engine lifetime

std::shared_mutex g_life; // exclusive: Start/Stop; shared: every call that touches the chain or the mixer
std::unique_ptr<CaptureChain> g_capture;
std::unique_ptr<Mixer> g_mixer;
HANDLE g_threads[2] = {};
HANDLE g_first_try[2] = {};          // set after each thread's first device attempt
std::atomic<int> g_device_ok[2] = {}; // 1 opened, 2 failed

template <typename T> void SafeRelease(T *&p) { if (p) { p->Release(); p = nullptr; } }

bool StopRequested(DWORD wait_ms) { return WaitForSingleObject(g_stop, wait_ms) == WAIT_OBJECT_0; }

void FirstTry(int which, bool ok) {
    if (g_device_ok[which].load() == 0) { g_device_ok[which] = ok ? 1 : 2; SetEvent(g_first_try[which]); }
    else if (ok) g_device_ok[which] = 1;
}

bool ParseFormat(const WAVEFORMATEX *w, dsp::DeviceFormat &f) {
    if (!w || !w->nChannels || !w->nSamplesPerSec || w->nSamplesPerSec > 384000) return false;
    WORD tag = w->wFormatTag, valid = w->wBitsPerSample;
    if (tag == WAVE_FORMAT_EXTENSIBLE && w->cbSize >= 22) {
        const auto *x = reinterpret_cast<const WAVEFORMATEXTENSIBLE *>(w);
        tag = static_cast<WORD>(x->SubFormat.Data1); // KSDATAFORMAT_SUBTYPE_PCM = 1, _IEEE_FLOAT = 3
        if (x->Samples.wValidBitsPerSample) valid = x->Samples.wValidBitsPerSample;
    }
    f.rate = w->nSamplesPerSec;
    f.channels = w->nChannels;
    f.container = static_cast<uint16_t>(w->wBitsPerSample / 8);
    if (w->nBlockAlign != f.container * f.channels) return false;
    if (tag == 3 && w->wBitsPerSample == 32) f.kind = dsp::SampleKind::Float32;
    else if (tag == 1 && w->wBitsPerSample == 16) f.kind = dsp::SampleKind::Int16;
    else if (tag == 1 && valid == 24 && (w->wBitsPerSample == 24 || w->wBitsPerSample == 32)) f.kind = dsp::SampleKind::Int24;
    else if (tag == 1 && w->wBitsPerSample == 32) f.kind = dsp::SampleKind::Int32;
    else return false;
    return true;
}

// One open capture session; returns when stopped or the device fails.
void CaptureSession(HANDLE ready) {
    IMMDeviceEnumerator *enumerator = nullptr;
    IMMDevice *device = nullptr;
    IAudioClient *client = nullptr;
    IAudioCaptureClient *capture = nullptr;
    WAVEFORMATEX *mix = nullptr;
    LPWSTR id = nullptr;
    dsp::DeviceFormat format;
    HRESULT hr = CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL, IID_PPV_ARGS(&enumerator));
    if (SUCCEEDED(hr)) hr = enumerator->GetDefaultAudioEndpoint(eCapture, eCommunications, &device);
    if (SUCCEEDED(hr)) hr = device->GetId(&id);
    if (SUCCEEDED(hr)) hr = device->Activate(__uuidof(IAudioClient), CLSCTX_ALL, nullptr, reinterpret_cast<void **>(&client));
    if (SUCCEEDED(hr)) hr = client->GetMixFormat(&mix);
    if (SUCCEEDED(hr) && !ParseFormat(mix, format)) hr = E_NOTIMPL;
    if (SUCCEEDED(hr)) hr = client->Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 200000, 0, mix, nullptr);
    if (SUCCEEDED(hr)) hr = client->SetEventHandle(ready);
    if (SUCCEEDED(hr)) hr = client->GetService(IID_PPV_ARGS(&capture));
    if (SUCCEEDED(hr)) hr = client->Start();
    static HRESULT last_error = S_OK;
    if (FAILED(hr)) {
        if (hr != last_error) Log("voice: no microphone (0x%08lx); retrying every 2 s", static_cast<unsigned long>(hr));
        last_error = hr;
        FirstTry(0, false);
    } else {
        last_error = S_OK;
        Log("voice: microphone open");
        FirstTry(0, true);
        g_capture->Restart();
        ULONGLONG last_check = GetTickCount64(), last_data = last_check;
        for (;;) {
            HANDLE waits[2] = {g_stop, ready};
            if (WaitForMultipleObjects(2, waits, FALSE, 500) == WAIT_OBJECT_0) break;
            UINT32 packet = 0;
            while (SUCCEEDED(hr = capture->GetNextPacketSize(&packet)) && packet) {
                last_data = GetTickCount64();
                BYTE *data = nullptr;
                UINT32 frames = 0;
                DWORD flags = 0;
                hr = capture->GetBuffer(&data, &frames, &flags, nullptr, nullptr);
                if (FAILED(hr)) break;
                g_capture->FeedDevice(format, data, frames, (flags & AUDCLNT_BUFFERFLAGS_SILENT) != 0);
                capture->ReleaseBuffer(frames);
                if (!g_running.load()) break;
            }
            if (FAILED(hr)) { Log("voice: microphone lost (0x%08lx); retrying", static_cast<unsigned long>(hr)); break; }
            if (!g_running.load()) break;
            // a live shared-mode stream always delivers (silence too); none for 2 s is a lost device that did not say
            // so (Wine/PulseAudio may not report AUDCLNT_E_DEVICE_INVALIDATED)
            if (GetTickCount64() - last_data > 2000) { Log("voice: microphone stalled; reopening"); break; }
            if (GetTickCount64() - last_check > 3000) { // the default communications microphone changed
                last_check = GetTickCount64();
                IMMDevice *now = nullptr;
                LPWSTR now_id = nullptr;
                const bool changed = SUCCEEDED(enumerator->GetDefaultAudioEndpoint(eCapture, eCommunications, &now)) &&
                    SUCCEEDED(now->GetId(&now_id)) && wcscmp(now_id, id) != 0;
                if (now_id) CoTaskMemFree(now_id);
                SafeRelease(now);
                if (changed) { Log("voice: default microphone changed"); break; }
            }
        }
        client->Stop();
    }
    if (mix) CoTaskMemFree(mix);
    if (id) CoTaskMemFree(id);
    SafeRelease(capture);
    SafeRelease(client);
    SafeRelease(device);
    SafeRelease(enumerator);
}

DWORD WINAPI CaptureThread(void *) {
    FlushDenormals();
    const HRESULT com = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    HANDLE ready = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    try {
        if (FAILED(com) || !ready) { FirstTry(0, false); Log("voice: capture thread has no COM/event"); }
        else
            while (g_running.load()) {
                CaptureSession(ready);
                if (!g_running.load() || StopRequested(kRetryMs)) break;
            }
    } catch (...) {
        Fail("the capture thread failed");
    }
    if (ready) CloseHandle(ready);
    if (SUCCEEDED(com)) CoUninitialize();
    FirstTry(0, false);
    return 0;
}

struct VoiceCallback final : IXAudio2VoiceCallback {
    HANDLE event = nullptr;
    void STDMETHODCALLTYPE OnVoiceProcessingPassStart(UINT32) override {}
    void STDMETHODCALLTYPE OnVoiceProcessingPassEnd() override {}
    void STDMETHODCALLTYPE OnStreamEnd() override {}
    void STDMETHODCALLTYPE OnBufferStart(void *) override {}
    void STDMETHODCALLTYPE OnBufferEnd(void *) override { SetEvent(event); }
    void STDMETHODCALLTYPE OnLoopEnd(void *) override {}
    void STDMETHODCALLTYPE OnVoiceError(void *, HRESULT) override { SetEvent(event); }
};

struct EngineCallback final : IXAudio2EngineCallback {
    HANDLE event = nullptr;
    std::atomic<bool> lost{false};
    void STDMETHODCALLTYPE OnProcessingPassStart() override {}
    void STDMETHODCALLTYPE OnProcessingPassEnd() override {}
    void STDMETHODCALLTYPE OnCriticalError(HRESULT) override { lost = true; SetEvent(event); }
};

// The default output's id (eConsole, what a default mastering voice uses), or empty: polled to follow a changed or
// lost output on both platforms (no notification client).
std::wstring DefaultRenderId(IMMDeviceEnumerator *enumerator) {
    std::wstring result;
    IMMDevice *device = nullptr;
    LPWSTR id = nullptr;
    if (enumerator && SUCCEEDED(enumerator->GetDefaultAudioEndpoint(eRender, eConsole, &device)) && SUCCEEDED(device->GetId(&id))) result = id;
    if (id) CoTaskMemFree(id);
    SafeRelease(device);
    return result;
}

void PlaybackSession(HMODULE library, HANDLE event) {
    IMMDeviceEnumerator *enumerator = nullptr;
    if (FAILED(CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL, IID_PPV_ARGS(&enumerator)))) enumerator = nullptr;
    const std::wstring output = DefaultRenderId(enumerator);
    ULONGLONG last_check = GetTickCount64();
    using Create = HRESULT(WINAPI *)(IXAudio2 **, UINT32, XAUDIO2_PROCESSOR);
    auto create = reinterpret_cast<Create>(GetProcAddress(library, "XAudio2Create"));
    IXAudio2 *engine = nullptr;
    IXAudio2MasteringVoice *master = nullptr;
    IXAudio2SourceVoice *voice = nullptr;
    VoiceCallback voice_cb;
    voice_cb.event = event;
    EngineCallback engine_cb;
    engine_cb.event = event;
    WAVEFORMATEX format{};
    format.wFormatTag = WAVE_FORMAT_IEEE_FLOAT;
    format.nChannels = 1;
    format.nSamplesPerSec = kRate;
    format.wBitsPerSample = 32;
    format.nBlockAlign = 4;
    format.nAvgBytesPerSec = kRate * 4;
    HRESULT hr = create ? create(&engine, 0, XAUDIO2_DEFAULT_PROCESSOR) : E_NOINTERFACE;
    if (SUCCEEDED(hr)) hr = engine->RegisterForCallbacks(&engine_cb);
    if (SUCCEEDED(hr)) hr = engine->CreateMasteringVoice(&master);
    if (SUCCEEDED(hr)) hr = engine->CreateSourceVoice(&voice, &format, 0, XAUDIO2_DEFAULT_FREQ_RATIO, &voice_cb);
    static HRESULT last_error = S_OK;
    if (FAILED(hr)) {
        if (hr != last_error) Log("voice: no audio output (0x%08lx); retrying every 2 s", static_cast<unsigned long>(hr));
        last_error = hr;
        FirstTry(1, false);
    } else {
        last_error = S_OK;
        FirstTry(1, true);
        Log("voice: playback open");
        static float buffers[kQueuedBuffers + 1][kFrame];
        int next = 0;
        bool started = false;
        while (!engine_cb.lost.load() && g_running.load()) {
            XAUDIO2_VOICE_STATE state{};
            voice->GetState(&state, XAUDIO2_VOICE_NOSAMPLESPLAYED);
            bool failed = false;
            for (UINT32 queued = state.BuffersQueued; queued < static_cast<UINT32>(kQueuedBuffers); queued++) {
                g_mixer->Pull(buffers[next], kFrame);
                XAUDIO2_BUFFER buffer{};
                buffer.AudioBytes = kFrame * sizeof(float);
                buffer.pAudioData = reinterpret_cast<const BYTE *>(buffers[next]);
                if (FAILED(voice->SubmitSourceBuffer(&buffer))) { failed = true; break; }
                next = (next + 1) % (kQueuedBuffers + 1); // one spare: a buffer is never rewritten while queued
            }
            if (failed) { Log("voice: playback submit failed; retrying"); break; }
            if (!started) { if (FAILED(voice->Start())) { Log("voice: playback start failed"); break; } started = true; }
            HANDLE waits[2] = {g_stop, event};
            if (WaitForMultipleObjects(2, waits, FALSE, 100) == WAIT_OBJECT_0) break;
            if (GetTickCount64() - last_check > 3000) {
                last_check = GetTickCount64();
                if (DefaultRenderId(enumerator) != output) { Log("voice: default audio output changed; reopening"); break; }
            }
        }
        if (engine_cb.lost.load()) Log("voice: audio output lost; retrying");
        voice->Stop();
        voice->FlushSourceBuffers();
    }
    if (voice) voice->DestroyVoice();
    if (master) master->DestroyVoice();
    if (engine) { engine->UnregisterForCallbacks(&engine_cb); engine->StopEngine(); engine->Release(); }
    SafeRelease(enumerator);
}

DWORD WINAPI PlaybackThread(void *) {
    FlushDenormals();
    const HRESULT com = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    HMODULE library = LoadLibraryExW(L"xaudio2_9.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    HANDLE event = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    try {
        if (FAILED(com) || !library || !event) { FirstTry(1, false); Log("voice: XAudio2 2.9 unavailable"); }
        else
            while (g_running.load()) {
                PlaybackSession(library, event);
                if (!g_running.load() || StopRequested(kRetryMs)) break;
            }
    } catch (...) {
        Fail("the playback thread failed");
    }
    if (event) CloseHandle(event);
    if (library) FreeLibrary(library);
    if (SUCCEEDED(com)) CoUninitialize();
    FirstTry(1, false);
    return 0;
}

void StopLocked() {
    g_running = false;
    if (g_stop) SetEvent(g_stop);
    for (HANDLE &t : g_threads)
        if (t) { WaitForSingleObject(t, 5000); CloseHandle(t); t = nullptr; }
    for (HANDLE &e : g_first_try)
        if (e) { CloseHandle(e); e = nullptr; }
    if (g_stop) { CloseHandle(g_stop); g_stop = nullptr; }
    g_capture.reset();
    g_mixer.reset();
    g_sending_until = 0;
}

bool CreateEngineLocked(SendFn send) {
    if (g_failed.load()) return false;
    try {
        auto capture = std::make_unique<CaptureChain>(std::move(send));
        auto mixer = std::make_unique<Mixer>();
        if (!capture->Init() || !mixer->Init()) { Fail("a codec library failed to start"); return false; }
        g_capture = std::move(capture);
        g_mixer = std::move(mixer);
        g_stop = CreateEventW(nullptr, TRUE, FALSE, nullptr);
        if (!g_stop) { Fail("no stop event"); g_capture.reset(); g_mixer.reset(); return false; }
        g_concealed = 0;
        g_running = true;
        return true;
    } catch (...) {
        Fail("the engine could not be created");
        g_capture.reset();
        g_mixer.reset();
        return false;
    }
}

} // namespace

bool Start(SendFn send) {
    try {
        std::unique_lock lock(g_life);
        if (g_running.load()) return true;
        StopLocked(); // a session that failed on its own: collect its threads
        FlushDenormals();
        if (!CreateEngineLocked(std::move(send))) return false;
        for (int i = 0; i < 2; i++) {
            g_device_ok[i] = 0;
            g_first_try[i] = CreateEventW(nullptr, TRUE, FALSE, nullptr);
        }
        g_threads[0] = CreateThread(nullptr, 0, CaptureThread, nullptr, 0, nullptr);
        g_threads[1] = CreateThread(nullptr, 0, PlaybackThread, nullptr, 0, nullptr);
        if (!g_threads[0] || !g_threads[1] || !g_first_try[0] || !g_first_try[1]) {
            Log("voice: engine threads unavailable");
            StopLocked();
            return false;
        }
        WaitForMultipleObjects(2, g_first_try, TRUE, 3000);
        const bool mic = g_device_ok[0].load() == 1, out = g_device_ok[1].load() == 1;
        if (!mic && !out) {
            Log("voice: no microphone and no audio output; voice stays off");
            StopLocked();
            return false;
        }
        if (!mic) Log("voice: listening only until a microphone appears");
        if (!out) Log("voice: talking only until an audio output appears");
        Log("voice: audio engine started (decoder complexity %d)", g_complexity.load());
        return true;
    } catch (...) {
        Log("voice: Start failed");
        return false;
    }
}

void Stop() {
    try {
        std::unique_lock lock(g_life);
        StopLocked();
    } catch (...) {
        Log("voice: Stop failed");
    }
}

bool Running() { return g_running.load(); }

void SetTransmit(bool allowed, bool push_to_talk) {
    g_ptt = push_to_talk;
    g_allowed = allowed;
    if (!allowed) g_sending_until = 0;
}

void SetBundle(int frames) { g_bundle = frames == 6 ? 6 : 3; }

void Receive(uint8_t seat, uint16_t seq, uint32_t ts, uint8_t flags, const uint8_t *opus, size_t size) {
    try {
        std::shared_lock lock(g_life);
        if (g_running.load() && g_mixer) g_mixer->Receive(seat, seq, ts, flags, opus, size);
    } catch (...) {
        Fail("Receive failed");
    }
}

void SetSilenced(uint8_t seat, bool silenced) {
    std::shared_lock lock(g_life);
    if (g_mixer) g_mixer->SetSilenced(seat, silenced);
}

void SetVolume(float voices) {
    std::shared_lock lock(g_life);
    if (g_mixer) g_mixer->SetVolume(voices);
}

bool SeatTalking(uint8_t seat) {
    std::shared_lock lock(g_life);
    return g_running.load() && g_mixer && g_mixer->Talking(seat);
}

bool Sending() { return g_running.load() && g_allowed.load() && NowMs() < g_sending_until.load(); }

namespace testing {

bool StartHeadless(SendFn send) {
    std::unique_lock lock(g_life);
    StopLocked();
    g_failed = false; // a test session starts clean
    return CreateEngineLocked(std::move(send));
}

void ProcessCapture(const float *interleaved, size_t frames, uint32_t rate, uint16_t channels) {
    std::shared_lock lock(g_life);
    if (!g_running.load() || !g_capture) return;
    FlushDenormals();
    dsp::DeviceFormat f{rate, channels, dsp::SampleKind::Float32, 4};
    try {
        g_capture->FeedDevice(f, reinterpret_cast<const uint8_t *>(interleaved), frames, false);
    } catch (...) {
        Fail("capture processing failed");
    }
}

size_t PullMix(float *out, size_t samples) {
    std::shared_lock lock(g_life);
    if (!g_running.load() || !g_mixer) { std::fill(out, out + samples, 0.0f); return 0; }
    FlushDenormals();
    try {
        return g_mixer->Pull(out, samples);
    } catch (...) {
        Fail("mixing failed");
        return 0;
    }
}

void SetClockMs(int64_t ms) { g_fake_now = ms; }
int DecoderComplexity() { return g_complexity.load(); }
void SetDecodeCostOverrideUs(double us) { g_cost_override = us; }
double DecodeCostP95Us() { return g_cost_p95.load(); }
uint64_t ConcealedFrames() { return g_concealed.load(); }

} // namespace testing
} // namespace fi::audio
