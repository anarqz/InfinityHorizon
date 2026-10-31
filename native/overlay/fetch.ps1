# Fetches the voice chat's third-party sources into third_party\ and refuses any other bytes: libopus 1.5.2, RNNoise
# 0.2, the WebRTC AudioProcessing module 2.1 (freedesktop's standalone release, for the AEC3 echo canceller) and the
# Abseil 20240722.0 it builds against, the release tarballs pinned by SHA-256. They are not in git (about 25 MB, mostly
# DNN weights as C sources). Nothing from the archives is run: build.ps1 compiles their C/C++ files with cl.
#   powershell -File native\overlay\fetch.ps1
$ErrorActionPreference = 'Stop'
$sources = @(
    @{ Name = 'opus-1.5.2'; Url = 'https://github.com/xiph/opus/releases/download/v1.5.2/opus-1.5.2.tar.gz'; Sha256 = '65c1d2f78b9f2fb20082c38cbe47c951ad5839345876e46941612ee87f9a7ce1'; Marker = 'COPYING' },
    @{ Name = 'rnnoise-0.2'; Url = 'https://github.com/xiph/rnnoise/releases/download/v0.2/rnnoise-0.2.tar.gz'; Sha256 = '90fce4b00b9ff24c08dbfe31b82ffd43bae383d85c5535676d28b0a2b11c0d37'; Marker = 'COPYING' },
    @{ Name = 'webrtc-audio-processing-2.1'; Url = 'https://freedesktop.org/software/pulseaudio/webrtc-audio-processing/webrtc-audio-processing-2.1.tar.xz'; Sha256 = 'ae9302824b2038d394f10213cab05312c564a038434269f11dbf68f511f9f9fe'; Marker = 'COPYING' },
    @{ Name = 'abseil-cpp-20240722.0'; Url = 'https://github.com/abseil/abseil-cpp/releases/download/20240722.0/abseil-cpp-20240722.0.tar.gz'; Sha256 = 'f50e5ac311a81382da7fa75b97310e4b9006474f9560ac46f54a9967f07d4ae3'; Marker = 'LICENSE' }
)
$third = Join-Path $PSScriptRoot 'third_party'
foreach ($s in $sources) {
    $target = Join-Path $third $s.Name
    if (Test-Path (Join-Path $target $s.Marker)) { continue }
    if (Test-Path $target) { Remove-Item -Recurse -Force $target } # a partial earlier fetch
    $work = Join-Path ([IO.Path]::GetTempPath()) ('fi-overlay-fetch-' + [Guid]::NewGuid().ToString('N')) # fresh and empty
    New-Item -ItemType Directory $work | Out-Null
    try {
        $archive = Join-Path $work ($s.Url -split '/')[-1]
        Invoke-WebRequest -UseBasicParsing -Uri $s.Url -OutFile $archive
        $hash = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
        if ($hash -ne $s.Sha256) { throw "$($s.Name): sha256 $hash is not the pinned $($s.Sha256)" }
        tar -xf $archive -C $work # bsdtar: .tar.gz and .tar.xz
        if ($LASTEXITCODE -ne 0) { throw "$($s.Name): tar failed" }
        Move-Item (Join-Path $work $s.Name) $target
        Write-Host "$($s.Name): fetched, sha256 ok"
    } finally {
        Remove-Item -Recurse -Force $work
    }
}
