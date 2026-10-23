// Copyright (C) 2026 Alexandre Moraes. GPL-3.0-only; see repository LICENSE.
#pragma once
#include "infinity_mod_host.h"
#include <windows.h>
#include <filesystem>
#include <string>
struct InfinityMod { std::string name; };
namespace infinity_mod {
constexpr uint32_t kGameSizeOfImage = 0x03A18000u;
void* find_game();
std::filesystem::path azimuth_dir();
std::filesystem::path game_mount();
std::filesystem::path extra_options_dir();
std::string narrow(const std::wstring&);
void log_line(const std::string&, const std::string&);
inline void plog(const std::string& text) { log_line("", text); }
void fill_hook_api(InfinityModApiV1&);
void notify_hook_fault(); // Nonblocking; failure is consumed by host status queries.
}
