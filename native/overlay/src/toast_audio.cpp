// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Original Infinity chime, embedded in the signed DLL. Audio work never runs on Present.
#include "overlay.h"
#include <xaudio2.h>
#include <cstring>
#include "fonts.h"

namespace fi {
namespace {
HANDLE g_sound = nullptr;

DWORD WINAPI AudioThread(void *module) {
    const HRESULT com = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    if (FAILED(com)) { Log("notification sound: COM unavailable"); return 0; }
    HMODULE library = LoadLibraryExW(L"xaudio2_9.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    using Create = HRESULT(WINAPI *)(IXAudio2 **, UINT32, XAUDIO2_PROCESSOR);
    auto create = library ? reinterpret_cast<Create>(GetProcAddress(library, "XAudio2Create")) : nullptr;
    IXAudio2 *engine = nullptr;
    IXAudio2MasteringVoice *master = nullptr;
    IXAudio2SourceVoice *voice = nullptr;
    HMODULE self = static_cast<HMODULE>(module);
    HRSRC resource = FindResourceW(self, MAKEINTRESOURCEW(FI_SOUND_NOTICE), RT_RCDATA);
    const DWORD size = resource ? SizeofResource(self, resource) : 0;
    const auto *wav = resource ? static_cast<const BYTE *>(LockResource(LoadResource(self, resource))) : nullptr;
    WAVEFORMATEX format{};
    format.wFormatTag = WAVE_FORMAT_PCM;
    format.nChannels = 1;
    format.nSamplesPerSec = 48000;
    format.wBitsPerSample = 16;
    format.nBlockAlign = 2;
    format.nAvgBytesPerSec = 96000;
    DWORD bytes = 0;
    if (wav && size > 44) std::memcpy(&bytes, wav + 40, sizeof(bytes));
    const bool valid = wav && size > 44 && bytes == size - 44 &&
        !std::memcmp(wav, "RIFF", 4) && !std::memcmp(wav + 8, "WAVE", 4) && !std::memcmp(wav + 36, "data", 4);
    if (valid && create && SUCCEEDED(create(&engine, 0, XAUDIO2_DEFAULT_PROCESSOR)) &&
        SUCCEEDED(engine->CreateMasteringVoice(&master)) && SUCCEEDED(engine->CreateSourceVoice(&voice, &format))) {
        voice->SetVolume(0.65f);
        XAUDIO2_BUFFER buffer{};
        buffer.Flags = XAUDIO2_END_OF_STREAM;
        buffer.AudioBytes = bytes;
        buffer.pAudioData = wav + 44;
        Log("notification sound: original Infinity chime ready");
        while (WaitForSingleObject(g_sound, INFINITE) == WAIT_OBJECT_0) {
            if (InPassthrough()) break;
            voice->Stop();
            voice->FlushSourceBuffers();
            if (SUCCEEDED(voice->SubmitSourceBuffer(&buffer)) && SUCCEEDED(voice->Start())) {
                if (DevHost()) Log("notification sound: started");
            } else {
                Log("notification sound: playback unavailable; visual toasts continue");
            }
        }
    } else {
        Log("notification sound unavailable; visual toasts continue");
    }
    if (voice) voice->DestroyVoice();
    if (master) master->DestroyVoice();
    if (engine) engine->Release();
    if (library) FreeLibrary(library);
    CoUninitialize();
    return 0;
}
}
void InitToastSound(HMODULE self) {
    g_sound = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    if (!g_sound) return;
    HANDLE worker = CreateThread(nullptr, 0, AudioThread, self, 0, nullptr);
    if (worker) CloseHandle(worker);
    else { CloseHandle(g_sound); g_sound = nullptr; Log("notification sound: worker unavailable"); }
}
void PlayToastSound() { if (g_sound) SetEvent(g_sound); }
}
