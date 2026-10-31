# Third-party sources (vendored, unmodified)

| Library | Version | Commit | License | Files kept |
| --- | --- | --- | --- | --- |
| Dear ImGui | v1.91.8 | dbb5eeaadffb6a3ba6a60de1290312e5802dba5a | MIT (`imgui/LICENSE.txt`) | core sources, `backends/imgui_impl_dx12.*`, `backends/imgui_impl_win32.*` |
| MinHook | v1.3.3 | 9fbd087432700d73fc571118d6a9697a36443d88 | BSD 2-clause (`minhook/LICENSE.txt`) | `src/`, `include/` |
| libopus | 1.5.2 | release tarball `opus-1.5.2.tar.gz` (SHA-256 65c1d2f78b9f2fb20082c38cbe47c951ad5839345876e46941612ee87f9a7ce1) | BSD 3-clause (`opus-1.5.2/COPYING`, also in NOTICE) | fetched by `../fetch.ps1`, not in git; `build.ps1` compiles the sources its `*_sources.mk` list for OSCE + deep PLC, never DRED |
| RNNoise | 0.2 | release tarball `rnnoise-0.2.tar.gz` (SHA-256 90fce4b00b9ff24c08dbfe31b82ffd43bae383d85c5535676d28b0a2b11c0d37) | BSD 3-clause (`rnnoise-0.2/COPYING`, also in NOTICE) | fetched by `../fetch.ps1`, not in git; ten `src/*.c` files |
| WebRTC AudioProcessing | 2.1 | release tarball `webrtc-audio-processing-2.1.tar.xz` from freedesktop.org (SHA-256 ae9302824b2038d394f10213cab05312c564a038434269f11dbf68f511f9f9fe, as published in its `.sha256sum`) | BSD 3-clause + WebRTC patent grant (`webrtc-audio-processing-2.1/webrtc/LICENSE`, `PATENTS`, also in NOTICE) | fetched by `../fetch.ps1`, not in git; `build.ps1` compiles only AEC3 (every `aec3/` source its meson.build lists) and the classes around it: AudioBuffer, band splitting, the high-pass |
| Abseil | 20240722.0 | release tarball `abseil-cpp-20240722.0.tar.gz` (SHA-256 f50e5ac311a81382da7fa75b97310e4b9006474f9560ac46f54a9967f07d4ae3, the hash WebRTC AudioProcessing's own meson wrap pins) | Apache 2.0 (`abseil-cpp-20240722.0/LICENSE`, also in NOTICE) | fetched by `../fetch.ps1`, not in git; headers only, no source compiled |
| miniz | 3.0.2 | release asset `miniz-3.0.2.zip` (SHA-256 ada38db0b703a56d3dd6d57bf84a9c5d664921d870d8fea4db153979fb5332c5) | MIT (`miniz/LICENSE.txt`) | the amalgamated `miniz.c`, `miniz.h` |

Fetched from https://github.com/ocornut/imgui, https://github.com/TsudaKageyu/minhook and
https://github.com/richgel999/miniz/releases (tags above). miniz's licence text is also in the
top-level NOTICE, which every package ships.
