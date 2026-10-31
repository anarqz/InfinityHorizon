// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: voice chat's audio engine (OD-154, VC-3, docs/launcher/VOICE_CHAT.md §3.2, §12.4, §13). Capture,
// processing, Opus, jitter buffers, decoding, mixing and playback, on the engine's own threads: never on the game's
// Present thread, never touching D3D12. Any failure turns voice off for the session and logs; the overlay and the
// game go on. No network here: voice.cpp frames what Send gives it and hands back what arrives.
#pragma once

#include "voice_spatial.h"

#include <cstddef>
#include <cstdint>
#include <functional>
#include <string>
#include <vector>

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
// Running with no microphone to use (none, or only monitor/loopback inputs): voice is received and played, nothing is
// captured. Rechecked every 2 s, so a microphone plugged in ends it. Any thread.
bool ListenOnly();

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

// The player's levels (VOICE_CHAT.md §15), linear gains in 0..1 (voice_volume.h's VolumeGain of a 1..100 slider). They
// outlive Stop/Start. The microphone's applies to the captured voice after the AGC and echo cancellation, before RNNoise
// and the encoder; a seat's applies to that talker's decoded voice before the mix (and before the mix's own level).
void SetMicGain(float gain);
void SetSeatGain(uint8_t seat, float gain);

// The endpoints the player chose (§15): IMMDevice ids in UTF-8, empty for the system default. A change reopens that
// stream at once (within ~0.5 s); a chosen device that is not there (unplugged, or for capture not a microphone) falls
// back to the system default until it comes back. Any thread. This is the only setter of the devices: the echo
// canceller's reference is the mix played to that same output, so the output it hears is always the one chosen here.
void SetDevices(const std::string &microphone_id, const std::string &output_id);
// The render endpoint voice plays to now (UTF-8 IMMDevice id: the chosen output, or the default's id while it stands
// in); empty while no output is open. For a WASAPI loopback of the playback device, should one ever be needed.
std::string OutputDeviceInUse();

// One entry of the settings' device lists: an empty id is "System default".
struct Device {
    std::string id, name; // UTF-8
};
// "System default" first, then each active endpoint in the system's order; for capture only microphones (never a
// monitor or loopback, voice_audio_devices.h). Enumerates through COM on the calling thread: never the game's thread.
std::vector<Device> ListDevices(bool capture);

// Heard talking now (a packet within the last 300 ms, not silenced). For the UI and the game's icons.
bool SeatTalking(uint8_t seat);
// SeatTalking and audible to this player now: in proximity a talker beyond range (gain under -60 dB) is talking but not
// heard. In open mode the same as SeatTalking. For the talk indicator in proximity (PROXIMITY_VOICE.md §5).
bool SeatHeard(uint8_t seat);

// Proximity voice (NR-7, PROXIMITY_VOICE.md; voice_spatial.h). The lobby's voice mode as the relay's state says it; Off
// and Open play every talker centred as before (the default), Proximity places each by its car. Any thread; it
// outlives Stop/Start. A change crossfades each playing talker over 200 ms.
using VoiceMode = spatial::Mode;
void SetVoiceMode(VoiceMode mode);
// The poses that came with a talker's voice down (the relay's pose attribute): the talker's car and this player's own,
// both sampled at the same relay instant. Call before Receive of the same packet. Any thread; copies; never blocks
// for long. Without poses (an older relay) proximity plays everyone open.
void SetPose(uint8_t seat, const spatial::Pose &talker, const spatial::Pose &self);
// The owner's listening-test knobs (range, r0, fade, Doppler scale s, pan limit, rear low-pass); out-of-range values
// are clamped (spatial::Sanitized). Any thread; it outlives Stop/Start.
void SetSpatialParams(const spatial::Params &params);
// This player's voice is leaving the PC now.
bool Sending();

} // namespace fi::audio
