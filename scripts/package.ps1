# Requires Windows PowerShell 5.1 or newer. Keep this file UTF-8 with BOM.
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$ClientOnly,
    [switch]$KeepVersion,
    [switch]$Offline,
    [ValidatePattern('^[a-z0-9][a-z0-9-]{0,31}$')]
    [string]$PackageLabel,
    [ValidatePattern('^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$')]
    [string]$Version,
    [ValidateNotNullOrEmpty()]
    [string]$ReleaseNotesPath
)

$ErrorActionPreference = 'Stop'
$OutputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false)
$PSDefaultParameterValues['*:Encoding'] = 'utf8'

if ($PSBoundParameters.ContainsKey('ReleaseNotesPath')) {
    $ReleaseNotesPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($ReleaseNotesPath)
}
$repoRoot = Split-Path -Parent $PSScriptRoot
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
$distRoot = Join-Path $repoRoot '.local\dist'
$targetRoot = Join-Path $repoRoot '.local\target'

if ($Version -and ($KeepVersion -or $SkipBuild)) {
    throw '-Version 不能与 -KeepVersion 或 -SkipBuild 同时使用。'
}
$manifestPath = Join-Path $repoRoot 'Cargo.toml'
$manifestText = [IO.File]::ReadAllText($manifestPath, [Text.Encoding]::UTF8)
$versionPattern = '(?m)(^\[workspace\.package\]\r?\nversion = ")([0-9]+\.[0-9]+\.[0-9]+)(")'
$versionMatch = [regex]::Match($manifestText, $versionPattern)
if (-not $versionMatch.Success) { throw '无法读取 Cargo.toml 中的 workspace 版本。' }
$currentVersion = $versionMatch.Groups[2].Value
if (-not $Version) {
    $Version = $currentVersion
    if (-not ($KeepVersion -or $SkipBuild)) {
        $parts = $currentVersion.Split('.')
        $Version = '{0}.{1}.{2}' -f $parts[0], $parts[1], ([long]$parts[2] + 1)
    }
}
if ([version]$Version -lt [version]$currentVersion) { throw '指定版本不能低于当前版本。' }
$appName = "NetBurrow-$Version"
if ($PackageLabel) {
    if ($ReleaseNotesPath) { throw '带特别版标记的包不能生成正式更新清单。' }
    $appName += "-$PackageLabel"
}
$appZip = Join-Path $distRoot "$appName-win-x64.zip"
$relayZip = Join-Path $distRoot "NetBurrow-$Version-relay-source.zip"
$appStage = Join-Path $distRoot "$appName-win-x64"
$relayStage = Join-Path $distRoot "NetBurrow-$Version-relay-source"

function New-ReleaseManifestJson {
    param([string]$NotesPath, [string]$ReleaseVersion)

    if (-not (Test-Path -LiteralPath $NotesPath -PathType Leaf)) {
        throw "更新说明文件不存在：$NotesPath"
    }
    if ((Get-Item -LiteralPath $NotesPath).Length -gt 65536) {
        throw '更新说明文件超过 64 KiB，请缩短说明。'
    }
    $strictUtf8 = [Text.UTF8Encoding]::new($false, $true)
    $notes = [IO.File]::ReadAllText($NotesPath, $strictUtf8)
    $characterCount = 0
    for ($i = 0; $i -lt $notes.Length; $i++) {
        $characterCount++
        if ([char]::IsHighSurrogate($notes[$i]) -and $i + 1 -lt $notes.Length -and [char]::IsLowSurrogate($notes[$i + 1])) { $i++ }
    }
    if ([string]::IsNullOrWhiteSpace($notes) -or $characterCount -gt 12000) {
        throw '更新说明不能为空，且不能超过 12000 个字符。'
    }
    $json = ([ordered]@{ version = $ReleaseVersion; notes = $notes } | ConvertTo-Json) + "`n"
    if ($strictUtf8.GetByteCount($json) -gt 65536) {
        throw '生成的更新清单超过 64 KiB，请缩短说明。'
    }
    return $json
}

# Validate before builds, version changes, or replacing existing package files.
$releaseManifestJson = $null
if ($PSBoundParameters.ContainsKey('ReleaseNotesPath')) {
    $releaseManifestJson = New-ReleaseManifestJson -NotesPath $ReleaseNotesPath -ReleaseVersion $Version
}
$licenseFiles = @('LICENSE', 'THIRD_PARTY_LICENSES.txt')
foreach ($name in $licenseFiles) {
    if (-not (Test-Path -LiteralPath (Join-Path $repoRoot $name) -PathType Leaf)) {
        throw "Missing required license file: $name"
    }
}

function Write-UpdateManifest {
    if ($null -eq $releaseManifestJson) { return }
    $updateDirectory = Join-Path $distRoot "updates\$Version"
    New-Item -ItemType Directory -Force -Path $updateDirectory | Out-Null
    $updateManifestPath = Join-Path $updateDirectory 'latest.json'
    [IO.File]::WriteAllText($updateManifestPath, $releaseManifestJson, [Text.UTF8Encoding]::new($false))
    Write-Output "已生成：$updateManifestPath"
    Write-Output '公开发行仓库：https://github.com/stmluyuer/NetBurrow-Releases'
    Write-Output "创建标签 v$Version 的 Draft Release，仅上传以下两个文件，确认附件齐全后发布为 Latest："
    Write-Output "  $appZip"
    Write-Output "  $updateManifestPath"
    Write-Output '不要上传 relay-source.zip、配置、日志或整个 dist 目录。'
}

function Remove-DistItem {
    param([Parameter(Mandatory = $true)][string]$Path)

    $fullPath = [IO.Path]::GetFullPath($Path)
    $fullDist = [IO.Path]::GetFullPath($distRoot).TrimEnd([IO.Path]::DirectorySeparatorChar, [IO.Path]::AltDirectorySeparatorChar)
    $distPrefix = $fullDist + [IO.Path]::DirectorySeparatorChar
    if ($fullPath -eq $fullDist -or -not $fullPath.StartsWith($distPrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "拒绝删除 dist 目录外的路径：$fullPath"
    }
    if (Test-Path -LiteralPath $fullPath) {
        Remove-Item -LiteralPath $fullPath -Force -Recurse
    }
}

function Invoke-Cargo {
    $Arguments = @($args)
    if ($Offline) { $Arguments += '--offline' }

    & $cargo @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "Cargo 命令失败：cargo $($Arguments -join ' ')"
    }
}

function Write-PortableZip {
    param([string]$Source, [string]$Destination)
    Add-Type -AssemblyName System.IO.Compression, System.IO.Compression.FileSystem
    $archive = [IO.Compression.ZipFile]::Open($Destination, [IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($file in Get-ChildItem -LiteralPath $Source -File -Recurse) {
            $entryName = $file.FullName.Substring($Source.Length + 1).Replace('\', '/')
            [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($archive, $file.FullName, $entryName, [IO.Compression.CompressionLevel]::Optimal) | Out-Null
        }
    } finally {
        $archive.Dispose()
    }
}

New-Item -ItemType Directory -Force -Path $distRoot | Out-Null

$appBinary = Join-Path $targetRoot 'x86_64-pc-windows-msvc\release\NetBurrow.exe'
$injectorBinary = Join-Path $targetRoot 'i686-pc-windows-msvc\release\netburrow-injector.exe'
$hookDll = Join-Path $targetRoot 'i686-pc-windows-msvc\release\netburrow_hook.dll'
$buildRecord = Join-Path $targetRoot 'package-build.json'
if ($Version -ne $currentVersion) {
    $updatedManifest = [regex]::Replace($manifestText, $versionPattern, ('${1}' + $Version + '${3}'))
    [IO.File]::WriteAllText($manifestPath, $updatedManifest, [Text.UTF8Encoding]::new($false))
    Write-Host "版本：$currentVersion -> $Version（失败后可使用 -KeepVersion 重试）"
}

if (-not $SkipBuild) {
    # Refresh workspace package versions in Cargo.lock without updating dependencies.
    Invoke-Cargo metadata --format-version 1 | Out-Null
    Invoke-Cargo build --release --locked --target-dir $targetRoot --target x86_64-pc-windows-msvc -p netburrow-app
    Invoke-Cargo build --release --locked --target-dir $targetRoot --target i686-pc-windows-msvc -p netburrow-injector -p netburrow-hook
    $hashes = @($appBinary, $injectorBinary, $hookDll | ForEach-Object { (Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash })
    @{ version = $Version; hashes = $hashes } | ConvertTo-Json | Set-Content -LiteralPath $buildRecord -Encoding UTF8
} else {
    if (-not (Test-Path -LiteralPath $buildRecord)) { throw '请先正常打包一次，再使用 -SkipBuild。' }
    $record = Get-Content -LiteralPath $buildRecord -Raw -Encoding UTF8 | ConvertFrom-Json
    $hashes = @($appBinary, $injectorBinary, $hookDll | ForEach-Object { (Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash })
    if ($record.version -ne $Version -or ($record.hashes -join ',') -ne ($hashes -join ',')) {
        throw '现有产物与记录的版本不一致，请使用 -KeepVersion 重新构建。'
    }
}

foreach ($artifact in @($appBinary, $injectorBinary, $hookDll)) {
    if (-not (Test-Path -LiteralPath $artifact -PathType Leaf)) {
        throw "缺少构建产物：$artifact"
    }
}

Remove-DistItem $appStage
Remove-DistItem $appZip
New-Item -ItemType Directory -Force -Path $appStage | Out-Null
Copy-Item -LiteralPath $appBinary -Destination (Join-Path $appStage 'NetBurrow.exe') -Force
Copy-Item -LiteralPath $injectorBinary -Destination (Join-Path $appStage 'netburrow-injector.exe') -Force
Copy-Item -LiteralPath $hookDll -Destination (Join-Path $appStage 'netburrow_hook.dll') -Force
foreach ($name in $licenseFiles) {
    Copy-Item -LiteralPath (Join-Path $repoRoot $name) -Destination (Join-Path $appStage $name) -Force
}
Get-ChildItem -LiteralPath $appStage -File -Recurse | Where-Object { $_.LastWriteTime.Year -lt 1980 -or $_.LastWriteTime.Year -gt 2107 } | ForEach-Object { $_.LastWriteTime = Get-Date }
Write-PortableZip -Source $appStage -Destination $appZip
Write-Output "已生成：$appZip"
if ($ClientOnly) {
    Write-UpdateManifest
    return
}

Remove-DistItem $relayStage
Remove-DistItem $relayZip
New-Item -ItemType Directory -Force -Path $relayStage | Out-Null
$relayManifest = @'
[workspace]
resolver = "3"
members = ["crates/netburrow-protocol", "crates/netburrow-relay"]
default-members = ["crates/netburrow-relay"]

[workspace.package]
version = "__PACKAGE_VERSION__"
edition = "2024"
publish = false
license = "MIT"

[workspace.dependencies]
netburrow-protocol = { path = "crates/netburrow-protocol" }
tokio = { version = "=1.52.3", features = ["rt-multi-thread", "macros", "net", "io-util", "sync", "time", "signal"] }
getrandom = "=0.3.4"

[profile.release]
lto = "thin"
codegen-units = 1
strip = "debuginfo"
'@
$relayManifest = $relayManifest.Replace('__PACKAGE_VERSION__', $Version)
Set-Content -LiteralPath (Join-Path $relayStage 'Cargo.toml') -Value $relayManifest -Encoding UTF8
New-Item -ItemType Directory -Force -Path (Join-Path $relayStage 'crates') | Out-Null
Copy-Item -LiteralPath (Join-Path $repoRoot 'crates\netburrow-protocol') -Destination (Join-Path $relayStage 'crates\netburrow-protocol') -Recurse -Force
Copy-Item -LiteralPath (Join-Path $repoRoot 'crates\netburrow-relay') -Destination (Join-Path $relayStage 'crates\netburrow-relay') -Recurse -Force
New-Item -ItemType Directory -Force -Path (Join-Path $relayStage 'docs') | Out-Null
Copy-Item -LiteralPath (Join-Path $repoRoot 'docs\server-ai-handoff.md') -Destination (Join-Path $relayStage 'docs\server-ai-handoff.md') -Force
Copy-Item -LiteralPath (Join-Path $repoRoot 'Cargo.lock') -Destination (Join-Path $relayStage 'Cargo.lock') -Force
foreach ($name in $licenseFiles) {
    Copy-Item -LiteralPath (Join-Path $repoRoot $name) -Destination (Join-Path $relayStage $name) -Force
}
Invoke-Cargo metadata --format-version 1 --manifest-path (Join-Path $relayStage 'Cargo.toml') | Out-Null
Get-ChildItem -LiteralPath $relayStage -File -Recurse | Where-Object { $_.LastWriteTime.Year -lt 1980 -or $_.LastWriteTime.Year -gt 2107 } | ForEach-Object { $_.LastWriteTime = Get-Date }
Write-PortableZip -Source $relayStage -Destination $relayZip

Write-Output "已生成：$relayZip"
Write-UpdateManifest
