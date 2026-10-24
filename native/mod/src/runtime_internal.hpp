// Copyright (C) 2026 Alexandre Moraes. GPL-3.0-only; see repository LICENSE.
#pragma once
#include "infinity_mod_host.h"
#include <windows.h>
#include <filesystem>
#include <string>
#include <vector>
struct InfinityMod { std::string name; };
namespace infinity_mod {
constexpr uint32_t kGameSizeOfImage = 0x03A18000u;
void* find_game();
std::filesystem::path azimuth_dir();
std::filesystem::path game_mount();
struct Readiness { std::wstring module, log; std::string success; };
void configure_readiness(std::vector<Readiness>);
std::string narrow(const std::wstring&);
void log_line(const std::string&, const std::string&);
void log_to(const std::wstring&);
inline void plog(const std::string& text) { log_line("", text); }
struct Fixup { uint32_t rva; uint8_t width; };
bool parse_relocations(const uint8_t* data, std::size_t size, uint32_t image_size, std::vector<Fixup>& out);
int compare_relocated(const uint8_t* disk, const uint8_t* live, std::size_t n, uint32_t rva, int64_t delta, const std::vector<Fixup>& fixups);
void fill_hook_api(InfinityModApiV1&);
void notify_hook_fault();
}
