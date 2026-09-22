$ErrorActionPreference = "Stop"

$projectRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location -LiteralPath $projectRoot

cargo test
if ($LASTEXITCODE -ne 0) { throw "cargo test failed" }

$previousRustFlags = $env:RUSTFLAGS
try {
    $env:RUSTFLAGS = "-C target-feature=+crt-static"
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed" }
}
finally {
    $env:RUSTFLAGS = $previousRustFlags
}

$distDir = Join-Path $projectRoot "dist"
New-Item -ItemType Directory -Force -Path $distDir | Out-Null
$sourceExe = Join-Path $projectRoot "target\release\comfyui-mcp-bridge.exe"
$targetExe = Join-Path $distDir "ComfyUI-MCP-Bridge.exe"
Copy-Item -LiteralPath $sourceExe -Destination $targetExe -Force

$hash = Get-FileHash -Algorithm SHA256 -LiteralPath $targetExe
Write-Host "Portable executable: $targetExe"
Write-Host "SHA256: $($hash.Hash)"
