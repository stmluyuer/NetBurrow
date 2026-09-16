$ErrorActionPreference = 'Stop'
$OutputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false)
$PSDefaultParameterValues['*:Encoding'] = 'utf8'
Set-Location (Split-Path $PSScriptRoot -Parent)
if (Test-Path (Join-Path $PWD '.local/rustup/settings.toml')) {
    $env:CARGO_HOME = Join-Path $PWD '.local/cargo'
    $env:RUSTUP_HOME = Join-Path $PWD '.local/rustup'
    $taskCargo = & rustup which cargo
    if ($LASTEXITCODE -ne 0) { throw 'Local Rust toolchain not found' }
    $taskBin = Split-Path -Parent $taskCargo
    $env:RUSTC = Join-Path $taskBin 'rustc.exe'
    $env:RUSTDOC = Join-Path $taskBin 'rustdoc.exe'
    $env:PATH = "$taskBin;" + $env:PATH
}
& cargo build -p netburrow-hook -p netburrow-injector --target i686-pc-windows-msvc --locked --offline
if ($LASTEXITCODE) { throw 'Native build failed' }
$fixture = Join-Path $PWD '.local/native-fixture'
New-Item -ItemType Directory -Path $fixture -Force | Out-Null
& rustc tests/native/steam_stub.rs --edition=2024 --target i686-pc-windows-msvc --crate-type cdylib --crate-name steam_api -C target-feature=+crt-static -o "$fixture/steam_api.dll"
if ($LASTEXITCODE) { throw 'Fixture ABI build failed' }
$deps = Join-Path $PWD '.local/target/i686-pc-windows-msvc/debug/deps'
$protocol = Get-ChildItem -LiteralPath $deps -Filter 'libnetburrow_protocol-*.rlib' | Sort-Object LastWriteTime -Descending | Select-Object -First 1
& rustc tests/native/host.rs --edition=2024 --target i686-pc-windows-msvc -C target-feature=+crt-static -L "dependency=$deps" --extern "netburrow_protocol=$($protocol.FullName)" -o "$fixture/isaac-ng.exe"
if ($LASTEXITCODE) { throw 'Fixture host build failed' }
& "$fixture/isaac-ng.exe" "$PWD/.local/target/i686-pc-windows-msvc/debug/netburrow-injector.exe" "$PWD/.local/target/i686-pc-windows-msvc/debug/netburrow_hook.dll"
if ($LASTEXITCODE) { throw 'Native fixture failed' }
