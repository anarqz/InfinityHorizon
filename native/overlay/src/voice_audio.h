// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: voice chat's audio engine (OD-147, VC-3, docs/launcher/VOICE_CHAT.md §3.2, §12.4, §13). Capture,
// processing, Opus, jitter buffers, decoding, mixing and playback, on the engine's own threads: never on the game's
// Present thread, never touching D3D12. Any failure turns voice off for the session and logs; the overlay and the
// game go on. No network here: voice.cpp frames what Send gives it and hands back what arrives.
#pragma once

#include <cstddef>
#include <cstdint>
#include <functional>

namespace fi::audio {

// One repacketized Opus packet of `frames` 20 ms frames (3, or 6 when the relay asks), ready for the wire. flags:
// bit 0 the last packet of a talk spurt, bit 1 sent while push-to-talk is held. seq counts packets; ts counts 20 ms
// frames since the engine started. Called on an engine thread; must not block.
using SendFn = std::function<void(const uint8_t *opus, size_t size, uint8_t flags, uint16_t seq, uint32_t ts)>;

// Starts capture and playback (WASAPI shared mode, 48 kHz mono inside; XAudio2 out). False: no device or a library
// failed; voice stays off and Start may be tried again later (a device plugged in).
bool Start(SendFn send);
void Stop();
bool Running();

// What may leave the PC now: false sends nothing (muted mic, voice off, no lobby voice, not subscribed). While true
// the engine still gates on voice activity (RNNoise, 300 ms hangover) unless push_to_talk is set: then the gate is
// open exactly while `true` (R3 held). Any thread.
void SetTransmit(bool allowed, bool push_to_talk);

// Frames per packet (3 or 6), as the relay's state says.
void SetBundle(int frames);

// A talker's packet from the relay (voice down): the seat names the talker. Any thread; copies; never blocks.
void Receive(uint8_t seat, uint16_t seq, uint32_t ts, uint8_t flags, const uint8_t *opus, size_t size);

// A seat this player must not hear (muted by them, for the lobby, blocked, deafened, or gone): its buffer is dropped
// and nothing more of it plays. The relay already enforces this; the engine is the second gate.
void SetSilenced(uint8_t seat, bool silenced);
void SetVolume(float voices); // 0..1, the mix's level

// Heard talking now (a packet within the last 300 ms, not silenced). For the UI and the game's icons.
bool SeatTalking(uint8_t seat);
// This player's voice is leaving the PC now.
bool Sending();

} // namespace fi::audio
