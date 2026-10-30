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
$includes = "/I`"$here\include`" /I`"$imgui`" /I`"$mh\include`" /I`"$mz`""
$sources = @(
    "$here\src\main.cpp", "$here\src\dx12.cpp", "$here\src\input.cpp", "$here\src\ui.cpp", "$here\src\pipe.cpp", "$here\src\keyboard.cpp", "$here\src\code_entry.cpp", "$here\src\worlds.cpp", "$here\src\pane.cpp", "$here\src\voice.cpp", "$here\src\toast_audio.cpp", "$here\src\png_fit.cpp", "$here\src\png_write.cpp",
    "$imgui\imgui.cpp", "$imgui\imgui_draw.cpp", "$imgui\imgui_tables.cpp", "$imgui\imgui_widgets.cpp",
    "$imgui\backends\imgui_impl_dx12.cpp", "$imgui\backends\imgui_impl_win32.cpp"
) | ForEach-Object { "`"$_`"" }
$csources = @("$mh\src\buffer.c", "$mh\src\hook.c", "$mh\src\trampoline.c", "$mh\src\hde\hde64.c") | ForEach-Object { "`"$_`"" }
$cmd = @"
call "$vcvars" >nul
cl /nologo /c /MT /O2 /W3 /I"$mh\include" $($csources -join ' ') /Fo"$out\obj\\" || exit /b 1
cl /nologo /c /MT $opt /W3 $mzdefs "$mz\miniz.c" /Fo"$out\obj\\" || exit /b 1
rc /nologo /fo "$out\obj\fonts.res" "$here\src\fonts.rc" || exit /b 1
cl $common $includes /LD $($sources -join ' ') "$out\obj\fonts.res" "$out\obj\buffer.obj" "$out\obj\hook.obj" "$out\obj\trampoline.obj" "$out\obj\hde64.obj" "$out\obj\miniz.obj" /Fo"$out\obj\\" /Fd"$out\obj\\" /Fe"$out\infinity-overlay.dll" /link /DEBUG /PDBALTPATH:infinity-overlay.pdb /OPT:REF /OPT:ICF user32.lib gdi32.lib ole32.lib || exit /b 1
"@
$bat = Join-Path $out 'build.cmd'
Set-Content -Path $bat -Value $cmd -Encoding ascii
cmd /c "`"$bat`""
if ($LASTEXITCODE -ne 0) { throw "build failed ($LASTEXITCODE)" }
Get-Item "$out\infinity-overlay.dll" | Select-Object Name, Length
