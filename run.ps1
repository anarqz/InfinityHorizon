# Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
[CmdletBinding()]
param([string]$Server = 'http://127.0.0.1:8480', [string]$Portal = 'http://127.0.0.1:3120')
$ErrorActionPreference = 'Stop'
$folder = Join-Path $PSScriptRoot 'artifacts/win-x64'
$key = [IO.File]::ReadAllText((Join-Path $folder 'development-key.txt')).Trim()
if ($key -notmatch '^[a-f0-9]{64}$') { throw 'Build a development package first with build.ps1' }
$start = [Diagnostics.ProcessStartInfo]::new((Join-Path $folder 'infinity.exe'))
$start.UseShellExecute = $false
$start.CreateNoWindow = $true
$start.WorkingDirectory = $folder
$start.Environment['FI_DEV_HOOK_KEY'] = $key
foreach ($argument in @('--environment', 'development', '--server', $Server, '--portal', $Portal)) { $start.ArgumentList.Add($argument) }
$process = [Diagnostics.Process]::Start($start)
$process.Dispose()
