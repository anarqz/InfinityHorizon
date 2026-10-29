# Third-party sources (vendored, unmodified)

| Library | Version | Commit | License | Files kept |
| --- | --- | --- | --- | --- |
| Dear ImGui | v1.91.8 | dbb5eeaadffb6a3ba6a60de1290312e5802dba5a | MIT (`imgui/LICENSE.txt`) | core sources, `backends/imgui_impl_dx12.*`, `backends/imgui_impl_win32.*` |
| MinHook | v1.3.3 | 9fbd087432700d73fc571118d6a9697a36443d88 | BSD 2-clause (`minhook/LICENSE.txt`) | `src/`, `include/` |
| miniz | 3.0.2 | release asset `miniz-3.0.2.zip` (SHA-256 ada38db0b703a56d3dd6d57bf84a9c5d664921d870d8fea4db153979fb5332c5) | MIT (`miniz/LICENSE.txt`) | the amalgamated `miniz.c`, `miniz.h` |

Fetched from https://github.com/ocornut/imgui, https://github.com/TsudaKageyu/minhook and
https://github.com/richgel999/miniz/releases (tags above). miniz's licence text is also in the
top-level NOTICE, which every package ships.
