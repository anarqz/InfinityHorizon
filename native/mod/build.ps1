param([string]$OutputDirectory)
$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$vs = & "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (!$vs) { throw 'Install Visual Studio C++ x64 tools and the Windows SDK.' }
$vcvars = Join-Path $vs 'VC/Auxiliary/Build/vcvars64.bat'
$out = if ($OutputDirectory) { [IO.Path]::GetFullPath($OutputDirectory) } else { Join-Path $here 'build' }
New-Item -ItemType Directory -Force $out, "$out/obj" | Out-Null
$mh = Join-Path $here 'third_party/minhook'
$batch = @"
@echo off
call "$vcvars" >nul
cl /nologo /c /MT /O2 /W3 /I"$mh/include" "$mh/src/buffer.c" "$mh/src/hook.c" "$mh/src/trampoline.c" "$mh/src/hde/hde64.c" /Fo"$out/obj/" || exit /b 1
cl /nologo /std:c++20 /utf-8 /EHsc /MT /O2 /W4 /WX /permissive- /DUNICODE /D_UNICODE /DWIN32_LEAN_AND_MEAN /DNOMINMAX /I"$here/include" /I"$mh/include" /I"$mh/src" /LD "$here/src/runtime.cpp" "$here/src/hooks.cpp" "$out/obj/buffer.obj" "$out/obj/hook.obj" "$out/obj/trampoline.obj" "$out/obj/hde64.obj" /Fo"$out/obj/" /Fe"$out/infinity_mod.dll" /link /Brepro /OPT:REF /OPT:ICF bcrypt.lib || exit /b 1
dumpbin /nologo /exports "$out/infinity_mod.dll" > "$out/exports.txt" || exit /b 1
"@
$bat = Join-Path $out 'build.cmd'
[IO.File]::WriteAllText($bat, $batch, [Text.Encoding]::ASCII)
& cmd /c "`"$bat`""
if ($LASTEXITCODE -ne 0) { throw 'Infinity mod runtime build failed.' }
Get-FileHash "$out/infinity_mod.dll" -Algorithm SHA256
