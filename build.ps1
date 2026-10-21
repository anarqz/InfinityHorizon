# Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Zig,
    [string]$Python = 'python',
    [string]$Version = '0.1.0',
    [string]$Output,
    [switch]$Production
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = $PSScriptRoot
$zigPath = (Resolve-Path -LiteralPath $Zig).Path
$devOutput = [IO.Path]::GetFullPath((Join-Path $root 'artifacts/win-x64'))
$out = if ($Output) { [IO.Path]::GetFullPath($Output) } elseif ($Production) { Join-Path $root 'artifacts/production/win-x64' } else { $devOutput }
if ($Production) {
    if ($out.TrimEnd('\','/').Equals($devOutput, [StringComparison]::OrdinalIgnoreCase) -or $out.StartsWith($devOutput + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Production output must be separate from the development package.'
    }
    if ((Test-Path -LiteralPath $out) -and (Get-ChildItem -LiteralPath $out -Force | Select-Object -First 1)) {
        throw 'Use a new empty production output directory.'
    }
}
$profile = if ($Production) { 'true' } else { 'false' }
New-Item -ItemType Directory -Force $out, "$out/hook", "$out/overlay", "$out/licenses" | Out-Null

# An existing signature must never survive a rebuild, including a switch to production mode.
foreach ($file in @("$out/infinity.exe.sig", "$out/infinity-updater.exe.sig", "$out/hook/infinity.dll.sig", "$out/overlay/infinity-overlay.dll.sig", "$out/development-key.txt", "$out/SHA256SUMS", "$out/SHA256SUMS.sig")) {
    if (Test-Path -LiteralPath $file) { Remove-Item -LiteralPath $file }
}

& dotnet publish "$root/src/Infinity.App/Infinity.App.csproj" -c Release -r win-x64 --self-contained true `
    -p:PublishSingleFile=true -p:IncludeNativeLibrariesForSelfExtract=true -p:DebugType=None -p:DebugSymbols=false "-p:Version=$Version" "-p:InfinityProduction=$profile" -o $out
if ($LASTEXITCODE -ne 0) { throw 'Launcher publish failed' }
& dotnet publish "$root/src/Infinity.Updater/Infinity.Updater.csproj" -c Release -r win-x64 --self-contained true `
    -p:PublishSingleFile=true -p:IncludeNativeLibrariesForSelfExtract=true -p:DebugType=None -p:DebugSymbols=false "-p:Version=$Version" "-p:InfinityProduction=$profile" -o $out
if ($LASTEXITCODE -ne 0) { throw 'Updater publish failed' }
$hookArgs = @("$root/native/hook/build.py", $zigPath, '--output', "$out/hook/infinity.dll")
if (!$Production) { $hookArgs += '--dev' }
& $Python @hookArgs
if ($LASTEXITCODE -ne 0) { throw 'Hook build failed' }
& "$root/native/overlay/build.ps1" -Release
Copy-Item -LiteralPath "$root/native/overlay/build/infinity-overlay.dll" -Destination "$out/overlay/infinity-overlay.dll" -Force

if (!$Production) {
    & dotnet build "$root/tools/DevSign/DevSign.csproj" -c Release --nologo
    if ($LASTEXITCODE -ne 0) { throw 'Development signer build failed' }
    $signer = "$root/tools/DevSign/bin/Release/net10.0/devsign.dll"
    & dotnet $signer sign "$out/infinity.exe"
    if ($LASTEXITCODE -ne 0) { throw 'Launcher development signing failed' }
    & dotnet $signer sign "$out/infinity-updater.exe"
    if ($LASTEXITCODE -ne 0) { throw 'Updater development signing failed' }
    & dotnet $signer sign "$out/hook/infinity.dll"
    if ($LASTEXITCODE -ne 0) { throw 'Hook development signing failed' }
    & dotnet $signer sign "$out/overlay/infinity-overlay.dll"
    if ($LASTEXITCODE -ne 0) { throw 'Overlay development signing failed' }
    $key = (& dotnet $signer key).Trim()
    if ($LASTEXITCODE -ne 0 -or $key -notmatch '^[a-f0-9]{64}$') { throw 'Development public key unavailable' }
    [IO.File]::WriteAllText("$out/development-key.txt", $key)
}
$licenses = @{
    'LICENSE' = 'LICENSE'; 'NOTICE' = 'NOTICE';
    'licenses/BouncyCastle-LICENSE.md' = 'licenses/BouncyCastle-LICENSE.md';
    'licenses/fh2-mods-LICENSE.txt' = 'licenses/fh2-mods-LICENSE.txt';
    'native/hook/NOTICE.md' = 'licenses/Hook-NOTICE.md';
    'native/overlay/third_party/imgui/LICENSE.txt' = 'licenses/ImGui-LICENSE.txt';
    'native/overlay/third_party/minhook/LICENSE.txt' = 'licenses/MinHook-LICENSE.txt';
    'native/overlay/third_party/fonts/Selawik-LICENSE.txt' = 'licenses/Selawik-LICENSE.txt';
    'native/overlay/third_party/fonts/FluentSystemIcons-LICENSE.txt' = 'licenses/FluentSystemIcons-LICENSE.txt'
}
foreach ($entry in $licenses.GetEnumerator()) { Copy-Item -LiteralPath (Join-Path $root $entry.Key) -Destination (Join-Path $out $entry.Value) -Force }
Write-Output "Built: $out"
if ($Production) { Write-Output 'Unsigned production binaries: maintainer signatures are required before distribution.' }
