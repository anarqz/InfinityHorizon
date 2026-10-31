# Builds infinity-overlay.dll with MSVC from Visual Studio.
#   powershell -File native\overlay\build.ps1 [-Release]
# Output: native\overlay\build\infinity-overlay.dll
param([switch]$Release, [string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { throw 'Visual Studio with the C++ tools was not found' }
$vcvars = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
$out = if ($OutputDirectory) { [IO.Path]::GetFullPath($OutputDirectory) } else { Join-Path $here 'build' }
New-Item -ItemType Directory -Force $out, "$out\obj" | Out-Null
$opt = if ($Release) { '/O2 /DNDEBUG' } else { '/O2 /Zi' }
$imgui = "$here\third_party\imgui"
$mh = "$here\third_party\minhook"
$mz = "$here\third_party\miniz"
$mzdefs = "/DMINIZ_NO_STDIO /DMINIZ_NO_TIME /DMINIZ_NO_ARCHIVE_APIS /DMINIZ_NO_ZLIB_COMPATIBLE_NAMES"
$common = "/nologo /std:c++20 /utf-8 /EHsc /MT /W4 /permissive- $opt /DUNICODE /D_UNICODE /DWIN32_LEAN_AND_MEAN /DNOMINMAX"
# Voice chat codecs (OD-154, VC-3): libopus 1.5.2 (OSCE + deep PLC, no DRED) and RNNoise 0.2. fetch.ps1 downloads the
# release tarballs pinned by SHA-256 (not in git); nothing from them is run, their C sources are compiled here.
$opus = "$here\third_party\opus-1.5.2"
$rnn = "$here\third_party\rnnoise-0.2"
# The echo canceller (release 2.0.0): WebRTC AudioProcessing 2.1 (freedesktop's standalone module) and the Abseil
# headers it needs (no Abseil source is compiled), fetched the same way.
$wap = "$here\third_party\webrtc-audio-processing-2.1"
$absl = "$here\third_party\abseil-cpp-20240722.0"
if (-not (Test-Path "$opus\COPYING") -or -not (Test-Path "$rnn\COPYING") -or -not (Test-Path "$wap\COPYING") -or -not (Test-Path "$absl\LICENSE")) { & "$here\fetch.ps1" }
$includes = "/I`"$here\include`" /I`"$imgui`" /I`"$mh\include`" /I`"$mz`" /I`"$opus\include`" /I`"$rnn\include`""
function Read-Mk([string[]]$files) { # libopus' own source lists, parsed as data
    $vars = @{}
    foreach ($f in $files) {
        $name = $null
        foreach ($line in Get-Content $f) {
            if ($line -match '^\s*([A-Z0-9_]+)\s*=\s*(.*)$') { $name = $Matches[1]; $vars[$name] = @(); $rest = $Matches[2] }
            elseif ($name) { $rest = $line }
            else { continue }
            $more = $rest.TrimEnd().EndsWith('\')
            foreach ($t in ($rest.TrimEnd('\', ' ', "`t") -split '\s+')) { if ($t -match '\.c$') { $vars[$name] += $t } }
            if (-not $more) { $name = $null }
        }
    }
    $vars
}
$mk = Read-Mk @("$opus\opus_sources.mk", "$opus\celt_sources.mk", "$opus\silk_sources.mk", "$opus\lpcnet_sources.mk")
$opusPlain = @('OPUS_SOURCES', 'OPUS_SOURCES_FLOAT', 'CELT_SOURCES', 'CELT_SOURCES_X86_RTCD', 'CELT_SOURCES_SSE', 'CELT_SOURCES_SSE2',
    'CELT_SOURCES_SSE4_1', 'SILK_SOURCES', 'SILK_SOURCES_FLOAT', 'SILK_SOURCES_X86_RTCD', 'SILK_SOURCES_SSE4_1',
    'DEEP_PLC_SOURCES', 'OSCE_SOURCES', 'DNN_SOURCES_X86_RTCD', 'DNN_SOURCES_SSE2', 'DNN_SOURCES_SSE4_1') | ForEach-Object { $mk[$_] }
$opusAvx2 = @('CELT_SOURCES_AVX2', 'SILK_SOURCES_AVX2', 'SILK_SOURCES_FLOAT_AVX2', 'DNN_SOURCES_AVX2') | ForEach-Object { $mk[$_] }
foreach ($s in @($opusPlain) + @($opusAvx2)) { if ($s -match 'dred|lossgen') { throw "refusing DRED/lossgen source $s" } }
$vobj = "$out\obj\voice"
New-Item -ItemType Directory -Force $vobj | Out-Null
$cfgh = "$vobj\opus_version.h" # a quoted /D define does not survive cl /MP's re-quoting
Set-Content -Path $cfgh -Value '#define PACKAGE_VERSION "1.5.2"' -Encoding ascii
$odefs = "/FI`"$cfgh`" /DOPUS_BUILD /DUSE_ALLOCA /DENABLE_DEEP_PLC /DENABLE_OSCE /DDISABLE_DEBUG_FLOAT /DOPUS_HAVE_RTCD " +
    '/DOPUS_X86_MAY_HAVE_SSE /DOPUS_X86_MAY_HAVE_SSE2 /DOPUS_X86_MAY_HAVE_SSE4_1 /DOPUS_X86_MAY_HAVE_AVX2 /DOPUS_X86_PRESUME_SSE /DOPUS_X86_PRESUME_SSE2 ' +
    "/I`"$opus\include`" /I`"$opus`" /I`"$opus\celt`" /I`"$opus\silk`" /I`"$opus\silk\float`" /I`"$opus\dnn`""
# RNNoise 0.2's tarball lacks the headers of its generic and SSE4.1 paths: its SSE2 path (vec_avx.h) is used.
$rdefs = "/DDISABLE_DEBUG_FLOAT /DOPUS_X86_MAY_HAVE_SSE2 /I`"$rnn\include`" /I`"$rnn\src`""
$rsrc = @('denoise.c', 'rnn.c', 'pitch.c', 'kiss_fft.c', 'celt_lpc.c', 'nnet.c', 'nnet_default.c', 'parse_lpcnet_weights.c', 'rnnoise_data.c', 'rnnoise_tables.c') | ForEach-Object { "src\$_" }
# AEC3 and the 100 Hz high-pass only: every aec3/ source the module's meson.build lists, and the few classes around it
# (AudioBuffer with its band splitting, the high-pass, field trials and checks). No noise suppressor, AGC, AECM or VAD,
# no logging (RTC_DISABLE_LOGGING), no debug dumps. AVX2 files are chosen at run time by cpuid, as in libopus.
$wapSrc = "$wap\webrtc"
$wapAec3 = [regex]::Matches((Get-Content -Raw "$wapSrc\modules\audio_processing\meson.build"), "'(aec3/[a-z0-9_]+\.cc)'") |
    ForEach-Object { "modules\audio_processing\$($_.Groups[1].Value)" }
$wapPlain = @($wapAec3 | Where-Object { $_ -notmatch '_avx2\.cc$' }) + @(
    'modules\audio_processing\audio_buffer.cc', 'modules\audio_processing\high_pass_filter.cc', 'modules\audio_processing\splitting_filter.cc',
    'modules\audio_processing\three_band_filter_bank.cc', 'modules\audio_processing\utility\cascaded_biquad_filter.cc',
    'modules\audio_processing\logging\apm_data_dumper.cc', 'api\audio\echo_canceller3_config.cc',
    'rtc_base\checks.cc', 'rtc_base\memory\aligned_malloc.cc', 'rtc_base\experiments\field_trial_parser.cc', 'rtc_base\platform_thread_types.cc',
    'rtc_base\race_checker.cc', 'rtc_base\string_encode.cc', 'system_wrappers\source\field_trial.cc', 'system_wrappers\source\metrics.cc',
    'system_wrappers\source\cpu_features.cc', 'common_audio\audio_util.cc', 'common_audio\resampler\push_sinc_resampler.cc',
    'common_audio\resampler\sinc_resampler.cc', 'common_audio\resampler\sinc_resampler_sse.cc', 'common_audio\third_party\ooura\fft_size_128\ooura_fft.cc',
    'common_audio\third_party\ooura\fft_size_128\ooura_fft_sse2.cc', 'common_audio\signal_processing\splitting_filter.c')
$wapAvx2 = @($wapAec3 | Where-Object { $_ -match '_avx2\.cc$' }) + @('common_audio\resampler\sinc_resampler_avx2.cc')
if ($wapPlain.Count -lt 70 -or $wapAvx2.Count -ne 6) { throw "unexpected WebRTC AEC3 source list ($($wapPlain.Count) + $($wapAvx2.Count))" }
$wdefs = "/std:c++20 /EHsc /utf-8 /wd4146 /wd4018 /DWEBRTC_WIN /DNOMINMAX /D_USE_MATH_DEFINES /DWIN32_LEAN_AND_MEAN /DNDEBUG " +
    "/DWEBRTC_ENABLE_AVX2 /DWEBRTC_APM_DEBUG_DUMP=0 /DRTC_DISABLE_LOGGING /DRTC_DISABLE_CHECK_MSG /I`"$wapSrc`" /I`"$absl`""
$codecLines = @()
$codecObjs = @()
# Response files: the source lists exceed cmd's line limit. "/Fo dir/": a trailing backslash breaks /MP's quoting.
foreach ($set in @(@('opus', $opus, $opusPlain, "$odefs"), @('opus_avx2', $opus, $opusAvx2, "$odefs /arch:AVX2"), @('rnnoise', $rnn, $rsrc, $rdefs),
        @('webrtc', $wapSrc, $wapPlain, $wdefs), @('webrtc_avx2', $wapSrc, $wapAvx2, "$wdefs /arch:AVX2"), @('webrtc_echo', "$here\src", @('voice_echo.cpp'), $wdefs))) {
    foreach ($g in ($set[2] | Group-Object { Split-Path $_ -Parent })) {
        $od = "$vobj\$($set[0])_" + ($g.Name -replace '[\\/]', '_')
        New-Item -ItemType Directory -Force $od | Out-Null
        $files = ($g.Group | ForEach-Object { "`"$(Join-Path $set[1] $_)`"" }) -join ' '
        Set-Content -Path "$od.rsp" -Value "/nologo /c /MT /O2 /W3 /MP /wd4244 /wd4267 /wd4305 /wd4996 $($set[3]) $files /Fo`"$od/`"" -Encoding ascii
        $codecLines += "cl @`"$od.rsp`" || exit /b 1"
        $codecObjs += $g.Group | ForEach-Object { "`"$od\$([IO.Path]::ChangeExtension((Split-Path $_ -Leaf), 'obj'))`"" }
    }
}
Set-Content -Path "$vobj\codecs.rsp" -Value ($codecObjs -join "`r`n") -Encoding ascii
$codecLines += "lib /nologo /OUT:`"$out\obj\voice_codecs.lib`" @`"$vobj\codecs.rsp`" || exit /b 1"
$sources = @(
    "$here\src\main.cpp", "$here\src\dx12.cpp", "$here\src\input.cpp", "$here\src\ui.cpp", "$here\src\pipe.cpp", "$here\src\keyboard.cpp", "$here\src\code_entry.cpp", "$here\src\worlds.cpp", "$here\src\pane.cpp", "$here\src\voice.cpp", "$here\src\voice_audio.cpp", "$here\src\toast_audio.cpp", "$here\src\png_fit.cpp", "$here\src\png_write.cpp",
    "$imgui\imgui.cpp", "$imgui\imgui_draw.cpp", "$imgui\imgui_tables.cpp", "$imgui\imgui_widgets.cpp",
    "$imgui\backends\imgui_impl_dx12.cpp", "$imgui\backends\imgui_impl_win32.cpp"
) | ForEach-Object { "`"$_`"" }
$csources = @("$mh\src\buffer.c", "$mh\src\hook.c", "$mh\src\trampoline.c", "$mh\src\hde\hde64.c") | ForEach-Object { "`"$_`"" }
$cmd = @"
call "$vcvars" >nul
$($codecLines -join "`r`n")
cl /nologo /c /MT /O2 /W3 /I"$mh\include" $($csources -join ' ') /Fo"$out\obj\\" || exit /b 1
cl /nologo /c /MT $opt /W3 $mzdefs "$mz\miniz.c" /Fo"$out\obj\\" || exit /b 1
rc /nologo /fo "$out\obj\fonts.res" "$here\src\fonts.rc" || exit /b 1
cl $common $includes /LD $($sources -join ' ') "$out\obj\fonts.res" "$out\obj\buffer.obj" "$out\obj\hook.obj" "$out\obj\trampoline.obj" "$out\obj\hde64.obj" "$out\obj\miniz.obj" "$out\obj\voice_codecs.lib" /Fo"$out\obj\\" /Fd"$out\obj\\" /Fe"$out\infinity-overlay.dll" /link /DEBUG /PDBALTPATH:infinity-overlay.pdb /OPT:REF /OPT:ICF user32.lib gdi32.lib ole32.lib || exit /b 1
"@
$bat = Join-Path $out 'build.cmd'
Set-Content -Path $bat -Value $cmd -Encoding ascii
cmd /c "`"$bat`""
if ($LASTEXITCODE -ne 0) { throw "build failed ($LASTEXITCODE)" }
Get-Item "$out\infinity-overlay.dll" | Select-Object Name, Length
