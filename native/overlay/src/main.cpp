// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Infinity overlay: the fi_overlay_start entry (include/fi_overlay.h, ABI 1), the log and the
// passthrough. Returns at once; the work runs on the overlay's own thread and in the game's
// Present. Any fault turns the overlay off and the game keeps running untouched.
#include "overlay.h"

#include <MinHook.h>

#include <atomic>
#include <cstdarg>
#include <cstdio>
#include <mutex>

#include "fi_overlay.h"

namespace fi {

namespace {
std::atomic<bool> g_started{false};
std::atomic<bool> g_passthrough{false};
std::mutex g_log_lock;
void (*g_host_log)(const char *) = nullptr;
int32_t (*g_livery_sync)(uint32_t) = nullptr; // the hook's, when it offers one (host size 48)
wchar_t g_log_path[MAX_PATH] = L"";
bool g_dev = false;
HMODULE g_self = nullptr;
wchar_t g_ui_pipe[128] = L"";

void WriteLogLine(const char *line) {
    if (g_host_log) {
        g_host_log(line);
        return;
    }
    std::lock_guard<std::mutex> lock(g_log_lock);
    HANDLE file = CreateFileW(g_log_path, FILE_APPEND_DATA, FILE_SHARE_READ, nullptr, OPEN_ALWAYS, 0, nullptr);
    if (file == INVALID_HANDLE_VALUE) {
        return;
    }
    DWORD written = 0;
    WriteFile(file, line, static_cast<DWORD>(strlen(line)), &written, nullptr);
    WriteFile(file, "\r\n", 2, &written, nullptr);
    CloseHandle(file);
}

DWORD WINAPI StartThread(void *) {
    InitToastSound(g_self);
    if (MH_Initialize() != MH_OK) {
        Passthrough("MinHook did not initialise");
        return 0;
    }
    if (!InstallRenderHooks()) {
        Passthrough("the D3D12 hooks could not be installed");
        return 0;
    }
    InstallGamepadHook(); // optional: without it the game also sees the pad while the sidebar is open
    StartPipe(g_ui_pipe);
    Log("hooks installed; waiting for the game's first frame");
    return 0;
}
} // namespace

void Log(const char *format, ...) {
    char line[512];
    va_list args;
    va_start(args, format);
    vsnprintf(line, sizeof(line), format, args);
    va_end(args);
    WriteLogLine(line);
}

void Passthrough(const char *why) {
    if (g_passthrough.exchange(true)) {
        return;
    }
    MH_DisableHook(MH_ALL_HOOKS);
    Log("passthrough: %s (the game runs without the overlay)", why);
    SendEvent(5, why);
}

bool InPassthrough() { return g_passthrough.load(); }

bool DevHost() { return g_dev; }

namespace {
std::atomic<uint64_t> g_ready_until{0};
}
void SetGameReadiness(bool ready) { g_ready_until = ready ? GetTickCount64() + 1500 : 0; }
bool GameReady() { return DevHost() || GetTickCount64() < g_ready_until.load(); }

int LiverySync(uint32_t timeout_ms) { return g_livery_sync ? g_livery_sync(timeout_ms) : FI_LIVERY_UNAVAILABLE; }

} // namespace fi

extern "C" __declspec(dllexport) int32_t fi_overlay_start(const FiOverlayHost *host) {
    if (!host) {
        return FI_OVERLAY_E_NULL;
    }
    if (host->abi != FI_OVERLAY_ABI) {
        return FI_OVERLAY_E_ABI;
    }
    if (host->size < FI_OVERLAY_HOST_V1_SIZE) { // an older hook (no livery_sync) is fine
        return FI_OVERLAY_E_SIZE;
    }
    if (!host->ui_pipe || wcsncmp(host->ui_pipe, L"\\\\.\\pipe\\ForzaInfinity-", 23) != 0) {
        return FI_OVERLAY_E_PIPE;
    }
    if (fi::g_started.exchange(true)) {
        return FI_OVERLAY_E_AGAIN;
    }
    // pinned: nothing can unload the code the game's Present now jumps into
    HMODULE self = nullptr;
    GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_PIN,
                       reinterpret_cast<LPCWSTR>(&fi_overlay_start), &self);
    fi::g_self = self;
    fi::g_host_log = host->log;
    fi::g_livery_sync = host->size >= sizeof(FiOverlayHost) ? host->livery_sync : nullptr;
    fi::g_dev = (host->flags & FI_OVERLAY_DEV) != 0;
    wcsncpy_s(fi::g_ui_pipe, host->ui_pipe, _TRUNCATE); // the hook's string is not ours to keep
    if (!fi::g_host_log) {
        GetModuleFileNameW(self, fi::g_log_path, MAX_PATH);
        if (wchar_t *slash = wcsrchr(fi::g_log_path, L'\\')) {
            wcscpy_s(slash + 1, MAX_PATH - (slash + 1 - fi::g_log_path), L"infinity-overlay.log");
        }
    }
    HANDLE thread = CreateThread(nullptr, 0, fi::StartThread, nullptr, 0, nullptr);
    if (!thread) {
        return FI_OVERLAY_E_PANIC;
    }
    CloseHandle(thread);
    return FI_OVERLAY_OK;
}

// Development host only (FI_OVERLAY_DEV): the test app drives the UI without a launcher.
extern "C" __declspec(dllexport) void fi_overlay_dev(int32_t command) {
    using namespace fi;
    if (!DevHost()) {
        return;
    }
    const bool pt = PRIMARYLANGID(GetUserDefaultUILanguage()) == LANG_PORTUGUESE;
    switch (command) {
    case 1:
        SetLink(Link::Online, "");
        PushToast(ToastKind::Success, pt ? "Conectado ao Infinity" : "Connected to Infinity", pt ? "Online · progresso salvo na nuvem" : "Online · progress saved to the cloud");
        break;
    case 2: PushToast(ToastKind::Update, pt ? "Atualização disponível" : "Update available", pt ? "Infinity 0.2 está pronto no launcher" : "Infinity 0.2 is ready in the launcher"); break;
    case 3: PushToast(ToastKind::Achievement, pt ? "Conquista desbloqueada" : "Achievement unlocked", pt ? "Primeira vitória online · 20 G" : "First online win · 20 G"); break;
    case 4: PushToast(ToastKind::Friend, pt ? "Pedido de amizade" : "Friend request", pt ? "RallyFox98 quer ser seu amigo" : "RallyFox98 wants to be your friend"); break;
    case 5:
        SetLink(Link::Offline, pt ? "servidor fora do ar" : "server unreachable");
        PushToast(ToastKind::Error, pt ? "Conexão perdida" : "Connection lost", pt ? "Jogando offline · tentando reconectar" : "Playing offline · trying to reconnect");
        break;
    case 6: SetProfile("Synthetic#00043", Tier::Supporter); break;
    case 7: SetProfile("Synthetic#00043", Tier::Free); break;
    case 8: ToggleSidebar(); break;
    case 9: Navigate(Nav::Down); break;
    case 10: Navigate(Nav::Right); break;
    case 11: DevRequest(1); break; // the keyboard, as a password
    case 12: DevRequest(2); break; // the keyboard, as a world name
    case 13: Navigate(Nav::Accept); break;
    case 14: Navigate(Nav::X); break;
    case 15: Navigate(Nav::Menu); break;
    case 16: Navigate(Nav::Up); break;
    case 17: DevRequest(3); break; // the world code
    case 18: Navigate(Nav::Back); break;
    default: break;
    }
}

BOOL WINAPI DllMain(HINSTANCE, DWORD, LPVOID) { return TRUE; }
