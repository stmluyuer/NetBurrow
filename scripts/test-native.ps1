# Requires Windows PowerShell 5.1 or newer. Keep this file UTF-8 with BOM.
[CmdletBinding()]
param([switch]$Offline)

$ErrorActionPreference = 'Stop'
$OutputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false)
$PSDefaultParameterValues['*:Encoding'] = 'utf8'
$repoRoot = Split-Path $PSScriptRoot -Parent
Set-Location -LiteralPath $repoRoot
# Use the repository toolchain only when its configured binaries are available.
# Otherwise preserve the user's normal Rust environment.
$cargo = $null
$localRustup = Join-Path $repoRoot '.local\rustup'
$localSettings = Join-Path $localRustup 'settings.toml'
if (Test-Path -LiteralPath $localSettings -PathType Leaf) {
    $settingsText = [IO.File]::ReadAllText($localSettings, [Text.Encoding]::UTF8)
    $toolchainMatch = [regex]::Match($settingsText, '(?m)^default_toolchain\s*=\s*"([^"/\\]+)"\s*$')
    if ($toolchainMatch.Success) {
        $localBin = Join-Path $localRustup ("toolchains\{0}\bin" -f $toolchainMatch.Groups[1].Value)
        $localCargo = Join-Path $localBin 'cargo.exe'
        if ((Test-Path -LiteralPath $localCargo -PathType Leaf) -and
            (Test-Path -LiteralPath (Join-Path $localBin 'rustc.exe') -PathType Leaf) -and
            (Test-Path -LiteralPath (Join-Path $localBin 'rustdoc.exe') -PathType Leaf)) {
            $cargo = $localCargo
            $env:CARGO_HOME = Join-Path $repoRoot '.local\cargo'
            $env:RUSTUP_HOME = $localRustup
            $env:RUSTC = Join-Path $localBin 'rustc.exe'
            $env:RUSTDOC = Join-Path $localBin 'rustdoc.exe'
            $env:PATH = "$localBin;$env:PATH"
        }
    }
}
if (-not $cargo) {
    $cargoCommand = Get-Command cargo.exe -ErrorAction SilentlyContinue
    if ($cargoCommand) {
        $cargo = $cargoCommand.Source
    } else {
        $userBin = Join-Path ([Environment]::GetFolderPath('UserProfile')) '.cargo\bin'
        $userCargo = Join-Path $userBin 'cargo.exe'
        if (Test-Path -LiteralPath $userCargo -PathType Leaf) {
            $cargo = $userCargo
            $env:PATH = "$userBin;$env:PATH"
        }
    }
}
if (-not $cargo) { throw 'Rust not found. Install Rust with the MSVC toolchain; see README.md.' }
$targetRoot = Join-Path $repoRoot '.local/target'
$buildArgs = @('build', '-p', 'netburrow-hook', '-p', 'netburrow-injector', '--target', 'i686-pc-windows-msvc', '--locked', '--target-dir', $targetRoot)
if ($Offline) { $buildArgs += '--offline' }
& $cargo @buildArgs
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
