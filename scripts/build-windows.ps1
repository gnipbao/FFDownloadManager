$ErrorActionPreference = 'Stop'
$project = Split-Path -Parent $PSScriptRoot
Set-Location $project
$version = (Get-Content 'src-tauri/tauri.conf.json' -Raw | ConvertFrom-Json).version
# Tauri builds only the desktop binary. Refresh the separate Web/CLI host too.
cargo build --release --locked --bin ffdm
if ($LASTEXITCODE -ne 0) { throw 'Web/CLI build failed' }
$cliVersion = & '.\target\release\ffdm.exe' --version
if ($LASTEXITCODE -ne 0 -or $cliVersion -ne "ffdm $version") {
  throw 'CLI and desktop versions do not match; refusing to package.'
}
if (-not (Test-Path 'src-tauri/resources/ffmpeg.exe')) {
  throw 'Run sh scripts/build-ffmpeg-windows.sh in MSYS2 UCRT64 first.'
}
npm ci
if ($LASTEXITCODE -ne 0) { throw 'npm ci failed' }
& '.\node_modules\.bin\tauri.cmd' build --bundles nsis -- --locked
if ($LASTEXITCODE -ne 0) { throw 'Tauri Windows build failed' }
& "$PSScriptRoot/check-windows-subsystem.ps1" -Path 'target/release/ffdownload-desktop.exe' -ExpectedSubsystem 2
& "$PSScriptRoot/check-windows-subsystem.ps1" -Path 'target/release/ffdm.exe' -ExpectedSubsystem 3
Write-Host 'Windows installer: target/release/bundle/nsis/'
Write-Host 'Web/CLI: target/release/ffdm.exe'
