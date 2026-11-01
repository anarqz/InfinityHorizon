// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the voice audio engine's test seam (VC-3). The same processing, codec, jitter buffers and mixer as
// Start(), with no device and no thread: the caller feeds capture buffers and pulls the mix. Used by the private
// tests only; Stop() ends it like a real session.
#pragma once

#include "voice_audio.h"
#include "voice_audio_devices.h"
#include "voice_audio_dsp.h"

#include <vector>

namespace fi::audio::testing {

// Starts the engine without WASAPI. False: a library failed.
bool StartHeadless(SendFn send);
// One capture buffer as a device would give it: interleaved float32, any rate and channel count.
void ProcessCapture(const float *interleaved, size_t frames, uint32_t rate, uint16_t channels);
// The same in any device format (int16, 24-in-32, uint8, ...), as the capture worker feeds the chain.
void ProcessCaptureFormat(const dsp::DeviceFormat &format, const uint8_t *data, size_t frames);
// The next `samples` of the 48 kHz mix as mono, (L + R) / 2: in open mode exactly the old mono mix.
size_t PullMix(float *out, size_t samples);
// The next `frames` of the 48 kHz stereo mix, interleaved L, R (what the output would play).
size_t PullMixStereo(float *interleaved, size_t frames);
// A fake millisecond clock for SeatTalking/Sending (< 0: the real clock).
void SetClockMs(int64_t ms);
// The decoders' complexity now: 7 (NoLACE) at the start everywhere; 6/5 only under measured decode load.
int DecoderComplexity();
// Replaces each mix frame's measured decode cost (microseconds per 20 ms, all talkers); < 0 measures for real.
void SetDecodeCostOverrideUs(double us);
// The p95 decode cost of the controller's last full ~3 s window (0 until full).
double DecodeCostP95Us();
// Concealed 20 ms frames so far, over every seat.
uint64_t ConcealedFrames();
// Each 10 ms capture block (48 kHz mono, [-1, 1]) before and after the echo canceller, on the capture path's thread;
// null stops it. For the echo bench (ERLE).
using EchoTap = void (*)(const float *microphone, const float *cancelled, int samples);
void SetEchoTap(EchoTap tap);
// Whether the mix is the echo canceller's far end (true, as in a game). The loopback bench plays a talker's own voice
// straight back, which no game does: to AEC3 that is endless double talk, so it turns this off.
void SetMixReference(bool on);

// A fake device backend for Start() (tests only): the engine's own device workers call it where they would call
// WASAPI, with the same selection rules, watchdog and lifetimes. Any call may block (a hung driver).
struct FakeDevices {
    virtual ~FakeDevices() = default;
    // The capture endpoints and the default's index, as CaptureCandidates gives them.
    virtual std::vector<devices::Candidate> CaptureList(int &default_index) = 0;
    // Opens the picked endpoint; false: it failed. `format` is the stream's format.
    virtual bool OpenCapture(const std::wstring &id, dsp::DeviceFormat &format) = 0;
    // The next buffer in `format` (interleaved bytes): frames read, 0 none now, < 0 the device is lost or its format
    // changed (the stream must be reopened).
    virtual int ReadCapture(std::vector<uint8_t> &buffer) = 0;
    virtual void CloseCapture() = 0;
    virtual bool OpenRender() = 0;
    // 10 ms of the 48 kHz stereo mix; false: the output is lost.
    virtual bool Render(const float *stereo, size_t frames) = 0;
    virtual void CloseRender() = 0;
};
// null: the real devices. Set before Start; it must outlive every worker that may still call it.
void SetFakeDevices(FakeDevices *devices);
// A device call that has not returned after this long is abandoned (default 8000 ms).
void SetStuckMs(int ms);
// The engine's worker threads still alive, abandoned ones included.
int WorkersAlive();

} // namespace fi::audio::testing
