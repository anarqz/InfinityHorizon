// GPL-3.0-only. Non-game process: runtime owns SQL and mod registration, launcher owns recovery.
#include <windows.h>
#include <cstdint>
#include <cwchar>
extern "C" __declspec(dllimport) int InfinityModsPrepareDatabaseV1(const wchar_t*, const uint8_t[32]);
int wmain(int argc, wchar_t** argv) {
    if (argc != 3 || wcslen(argv[2]) != 64) return 2;
    uint8_t hash[32]{};
    for (unsigned i = 0; i < 64; ++i) {
        wchar_t c = argv[2][i]; unsigned n = c >= L'0' && c <= L'9' ? unsigned(c-L'0') : c >= L'a' && c <= L'f' ? unsigned(c-L'a'+10) : 16;
        if (n > 15) return 2; hash[i/2] = uint8_t(hash[i/2] * 16 + n);
    }
    return InfinityModsPrepareDatabaseV1(argv[1], hash);
}
