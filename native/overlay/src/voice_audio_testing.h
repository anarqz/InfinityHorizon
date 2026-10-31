// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the voice audio engine's test seam (VC-3). The same processing, codec, jitter buffers and mixer as
// Start(), with no device and no thread: the caller feeds capture buffers and pulls the mix. Used by the private
// tests only; Stop() ends it like a real session.
#pragma once

#include "voice_audio.h"

namespace fi::audio::testing {

// Starts the engine without WASAPI or XAudio2. False: a library failed.
bool StartHeadless(SendFn send);
// One capture buffer as a device would give it: interleaved float32, any rate and channel count.
void ProcessCapture(const float *interleaved, size_t frames, uint32_t rate, uint16_t channels);
// The next `samples` of the 48 kHz mono mix (what the source voice would play).
size_t PullMix(float *out, size_t samples);
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

} // namespace fi::audio::testing
