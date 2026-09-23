$ErrorActionPreference = 'Stop'
$project = Split-Path -Parent $PSScriptRoot
Set-Location $project
if (-not (Test-Path 'src-tauri/resources/ffmpeg.exe')) {
  throw 'Run sh scripts/build-ffmpeg-windows.sh in MSYS2 UCRT64 first.'
}
npm ci
if ($LASTEXITCODE -ne 0) { throw 'npm ci failed' }
& '.\node_modules\.bin\tauri.cmd' build --bundles nsis -- --locked
if ($LASTEXITCODE -ne 0) { throw 'Tauri Windows build failed' }
Write-Host 'Windows installer: target/release/bundle/nsis/'
