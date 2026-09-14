# Requires Windows PowerShell 5.1 or newer. Keep this file UTF-8 with BOM.
[CmdletBinding()]
param(
    [switch]$NonInteractive,
    [switch]$KeepVersion,
    [ValidatePattern('^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$')]
    [string]$Version
)

$ErrorActionPreference = 'Stop'
$OutputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
[Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false)
$PSDefaultParameterValues['*:Encoding'] = 'utf8'

$packageScript = Join-Path $PSScriptRoot 'scripts\package.ps1'
$distRoot = Join-Path $PSScriptRoot '.local\dist'
$exitCode = 0

try {
    Write-Host '正在打包 NetBurrow 客户端，请等待编译和压缩完成……' -ForegroundColor Cyan
    Write-Host '首次构建可能较慢；后续运行会自动复用未变更的编译结果。'
    $packageArgs = @{ ClientOnly = $true; KeepVersion = $KeepVersion }
    if ($Version) { $packageArgs.Version = $Version }
    & $packageScript @packageArgs
    Write-Host ''
    Write-Host '打包成功！文件路径见上方“已生成”，产物目录：' -ForegroundColor Green
    Write-Host $distRoot
    if (-not $NonInteractive) {
        try {
            Start-Process -FilePath 'explorer.exe' -ArgumentList ('"{0}"' -f $distRoot) | Out-Null
        } catch {
            Write-Warning "无法打开产物目录，请按上面的路径手动打开：$($_.Exception.Message)"
        }
    }
} catch {
    $exitCode = 1
    Write-Host ''
    Write-Host "打包失败：$($_.Exception.Message)" -ForegroundColor Red
    Write-Host '请查看上方错误信息，解决后重新运行脚本。'
} finally {
    if (-not $NonInteractive) {
        [void](Read-Host '按回车关闭窗口')
    }
}

exit $exitCode
