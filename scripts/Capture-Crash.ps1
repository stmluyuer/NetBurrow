# Requires Windows PowerShell 5.1. Keep this file UTF-8 with BOM.
[CmdletBinding()]
param(
    [string]$GamePath,
    [string]$PackageDirectory = $PSScriptRoot,
    [string]$ProcDumpPath,
    [string]$OutputRoot = (Join-Path $env:LOCALAPPDATA 'NetBurrow\crashes'),
    [switch]$AcceptEula,
    [switch]$Managed,
    [string]$ControlDirectory,
    [int]$ParentPid,
    [long]$ParentCreated
)

$ErrorActionPreference = 'Stop'
$OutputEncoding = [Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
[Console]::InputEncoding = [Text.UTF8Encoding]::new($false)
$PSDefaultParameterValues['*:Encoding'] = 'utf8'
$captureProcess = $null
$targetProcess = $null
$parentProcess = $null
$sessionPath = $null
$lastCapturePath = $null
$lastPublishedStatus = $null
$cancelRequested = $false
$sessionComplete = $false
$resultCode = 1
$metadata = [ordered]@{ started_at = [DateTimeOffset]::Now.ToString('o'); status = 'starting' }

function Publish-Status([string]$State, [string]$Message, [switch]$BestEffort) {
    if (-not $Managed -or -not $ControlDirectory) { return }
    $status = [ordered]@{ state = $State; message = $Message; capture_path = $script:lastCapturePath }
    $json = $status | ConvertTo-Json -Compress
    if ($json -eq $script:lastPublishedStatus) { return }
    $temporaryPath = Join-Path $ControlDirectory 'status.tmp'
    $statusPath = Join-Path $ControlDirectory 'status.json'
    try {
        [IO.File]::WriteAllText($temporaryPath, $json, [Text.UTF8Encoding]::new($false))
        if ([IO.File]::Exists($statusPath)) {
            [IO.File]::Replace($temporaryPath, $statusPath, [NullString]::Value)
        } else {
            [IO.File]::Move($temporaryPath, $statusPath)
        }
        $script:lastPublishedStatus = $json
    } catch {
        if (-not $BestEffort) { throw }
        Write-Warning ('无法更新状态，但会继续安全解除监视：' + $_.Exception.Message)
    }
}

function Test-Cancelled {
    if (-not $Managed) { return $false }
    if ($script:cancelRequested) { return $true }
    if ((Test-Path -LiteralPath (Join-Path $ControlDirectory 'stop')) -or ($parentProcess -and $parentProcess.HasExited)) {
        $script:cancelRequested = $true
    }
    return $script:cancelRequested
}

function Assert-NotCancelled {
    if (Test-Cancelled) { throw [OperationCanceledException]::new('已请求停止自动记录。') }
}

function Wait-Cancellable([int]$Milliseconds) {
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while ($timer.ElapsedMilliseconds -lt $Milliseconds) {
        Assert-NotCancelled
        Start-Sleep -Milliseconds ([Math]::Min(500, $Milliseconds - [int]$timer.ElapsedMilliseconds))
    }
    Assert-NotCancelled
}

function Save-Metadata {
    $metadata | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $sessionPath 'capture.json') -Encoding UTF8
}

function Copy-Logs([string]$Phase) {
    $destination = Join-Path $sessionPath $Phase
    New-Item -ItemType Directory -Force -Path $destination | Out-Null
    $sources = @()
    foreach ($name in 'client', 'injector', 'hook', 'network') {
        foreach ($suffix in '.log', '.previous.log') {
            $sources += @{ Source = (Join-Path $env:LOCALAPPDATA "NetBurrow\logs\$name$suffix"); Name = "$name$suffix" }
        }
    }
    $documents = [Environment]::GetFolderPath('MyDocuments')
    foreach ($edition in 'Repentance+', 'Repentance', 'Rebirth') {
        $sources += @{ Source = (Join-Path $documents "My Games\Binding of Isaac $edition\log.txt"); Name = "isaac-$edition.log" }
    }
    foreach ($item in $sources) {
        Assert-NotCancelled
        $inputStream = $null
        $outputStream = $null
        try {
            if (-not (Test-Path -LiteralPath $item.Source -PathType Leaf)) { continue }
            $inputStream = [IO.File]::Open($item.Source, [IO.FileMode]::Open, [IO.FileAccess]::Read, ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
            $outputStream = [IO.File]::Create((Join-Path $destination $item.Name))
            $buffer = New-Object byte[] 65536
            while (($bytesRead = $inputStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                Assert-NotCancelled
                $outputStream.Write($buffer, 0, $bytesRead)
            }
        } catch {
            if ($_.Exception -is [OperationCanceledException]) { throw }
            Add-Content -LiteralPath (Join-Path $sessionPath 'collection-warnings.txt') -Value "$Phase / $($item.Name): $($_.Exception.Message)" -Encoding UTF8
        } finally {
            if ($outputStream) { $outputStream.Dispose() }
            if ($inputStream) { $inputStream.Dispose() }
        }
    }
}

function Assert-MicrosoftProcDump([string]$Path) {
    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch '(^|,\s*)O=Microsoft Corporation(,|$)') {
        throw "ProcDump 的微软数字签名验证失败，已停止：$Path"
    }
    if ([Diagnostics.FileVersionInfo]::GetVersionInfo($Path).OriginalFilename -ine 'procdump') {
        throw '指定文件不是微软 ProcDump。请使用官方压缩包中的 procdump.exe。'
    }
}

function Read-DumpInfo([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    $reader = [IO.BinaryReader]::new($stream)
    try {
        if ($stream.Length -lt 32 -or $reader.ReadUInt32() -ne 0x504d444d) { throw '转储文件头无效。' }
        $stream.Position = 8
        $streamCount = $reader.ReadUInt32()
        $directory = $reader.ReadUInt32()
        $stream.Position = 24
        $flags = $reader.ReadUInt64()
        $exceptionCode = $null
        for ($index = 0; $index -lt $streamCount; $index++) {
            $stream.Position = $directory + 12 * $index
            $type = $reader.ReadUInt32()
            $size = $reader.ReadUInt32()
            $rva = $reader.ReadUInt32()
            if ($type -eq 6 -and $size -ge 12) {
                $stream.Position = $rva + 8
                $exceptionCode = '0x{0:X8}' -f $reader.ReadUInt32()
            }
        }
        return [ordered]@{ file = [IO.Path]::GetFileName($Path); bytes = $stream.Length; flags = ('0x{0:X}' -f $flags); full_memory = (($flags -band 2) -ne 0); exception_code = $exceptionCode }
    } finally {
        $reader.Dispose()
    }
}

function Find-GameProcess {
    $matches = @()
    $readFailed = $false
    foreach ($candidate in @(Get-Process -Name 'isaac-ng' -ErrorAction SilentlyContinue)) {
        $keep = $false
        try {
            $null = $candidate.Handle # Pin the instance before inspecting its executable.
            if (-not $candidate.HasExited -and $candidate.MainModule.FileName -ieq $GamePath) {
                $matches += $candidate
                $keep = $true
            }
        } catch {
            $readFailed = $true
        } finally {
            if (-not $keep) { $candidate.Dispose() }
        }
    }
    if ($Managed -and $matches.Count -gt 1) {
        $sameSession = @($matches | Where-Object { $_.SessionId -eq $parentProcess.SessionId })
        if ($sameSession.Count -gt 0) {
            foreach ($candidate in $matches) {
                if ($candidate.SessionId -ne $parentProcess.SessionId) { $candidate.Dispose() }
            }
            $matches = $sameSession
        }
    }
    if ($matches.Count -gt 1) {
        foreach ($candidate in $matches) { $candidate.Dispose() }
        throw '同一路径有多个游戏进程，请只保留一个；关闭多余进程后会自动继续。'
    }
    if ($matches.Count -eq 1) { return $matches[0] }
    if ($readFailed) { throw '无法读取游戏进程。若游戏以管理员运行，请也以管理员运行 NetBurrow 或采集脚本。' }
    return $null
}

function New-CaptureSession {
    $script:sessionComplete = $false
    $script:sessionPath = Join-Path $OutputRoot ((Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
    New-Item -ItemType Directory -Path $sessionPath -Force | Out-Null
    $script:metadata = [ordered]@{ started_at = [DateTimeOffset]::Now.ToString('o'); status = 'starting' }
    $metadata.game_path = $GamePath
    $metadata.game_sha256 = (Get-FileHash -LiteralPath $GamePath -Algorithm SHA256).Hash
    $metadata.game_version = [Diagnostics.FileVersionInfo]::GetVersionInfo($GamePath).FileVersion
    $metadata.procdump_version = [Diagnostics.FileVersionInfo]::GetVersionInfo($ProcDumpPath).FileVersion
    $metadata.package_files = @()
    foreach ($name in 'NetBurrow.exe', 'netburrow-injector.exe', 'netburrow_hook.dll') {
        Assert-NotCancelled
        $file = Join-Path $PackageDirectory $name
        if (Test-Path -LiteralPath $file -PathType Leaf) {
            $metadata.package_files += @{ name = $name; sha256 = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash }
        }
    }
    $metadata.status = 'waiting_for_game'
    Save-Metadata
    Write-Host "保存目录：$sessionPath"
}

function Stop-CaptureProcess {
    if (-not $captureProcess -or $captureProcess.HasExited -or -not $targetProcess) { return }
    if ($metadata.status -ne 'failed') { $metadata.status = 'cancelled' }
    Publish-Status 'stopping' '正在安全解除异常监视，请稍候。' -BestEffort
    # Never kill the debugger: doing so can terminate the attached game.
    $cancelInfo = [Diagnostics.ProcessStartInfo]::new()
    $cancelInfo.FileName = $ProcDumpPath
    $cancelInfo.Arguments = "-accepteula -cancel $($targetProcess.Id)"
    $cancelInfo.UseShellExecute = $false
    $cancelInfo.CreateNoWindow = $true
    $cancelInfo.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden
    $cancelProcess = $null
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $lastCancel = -10000L
    try {
        while (-not $captureProcess.HasExited) {
            # A stop can race the initial attach. Repeat the documented detach signal
            # only after the previous signal process exits; never replace it with Kill.
            if (($timer.ElapsedMilliseconds - $lastCancel) -ge 10000 -and (-not $cancelProcess -or $cancelProcess.HasExited)) {
                if ($cancelProcess) { $cancelProcess.Dispose(); $cancelProcess = $null }
                try {
                    $cancelProcess = [Diagnostics.Process]::Start($cancelInfo)
                    $null = $cancelProcess.Handle
                } catch {
                    Publish-Status 'failed' ('解除监视失败，正在等待安全退出：' + $_.Exception.Message) -BestEffort
                }
                $lastCancel = $timer.ElapsedMilliseconds
            }
            if ($timer.ElapsedMilliseconds -ge 10000) {
                Publish-Status 'failed' '监视尚未解除，后台会继续等待安全退出。请正常退出游戏，不要结束采集进程。' -BestEffort
            }
            Start-Sleep -Milliseconds 500
        }
        $captureProcess.WaitForExit()
    } finally {
        if ($cancelProcess) { $cancelProcess.Dispose() }
    }
}

function Finish-CaptureSession {
    if (-not $sessionPath -or $sessionComplete) { return }
    if ($targetProcess -and $targetProcess.HasExited) {
        $metadata.game_exit_code = '0x{0:X8}' -f $targetProcess.ExitCode
    }
    Copy-Logs 'after'
    Assert-NotCancelled
    $metadata.finished_at = [DateTimeOffset]::Now.ToString('o')
    Save-Metadata
    if ($metadata.status -eq 'exception_captured') {
        Assert-NotCancelled
        # The marker seals this directory: no subsequent code writes inside it.
        $marker = [IO.File]::Create((Join-Path $sessionPath 'capture.complete'))
        $marker.Dispose()
        $script:sessionComplete = $true
        $script:lastCapturePath = $sessionPath
        Publish-Status 'captured' '已保存异常现场和日志，正在整理排查压缩包。'
    }
    Write-Host "本次记录：$sessionPath"
}

function Capture-GameInstance {
    $metadata.pid = $targetProcess.Id
    $metadata.process_started_at = $targetProcess.StartTime.ToString('o')
    Copy-Logs 'before'
    $arguments = @('-accepteula', '-ma', '-e', '1', '-f', 'C0000005,C0000409,C0000374', '-n', '1', "$($targetProcess.Id)", ('"' + $sessionPath + '"'))
    $metadata.capture_arguments = $arguments
    $metadata.status = 'attaching'
    Save-Metadata
    Publish-Status 'attaching' '正在接入游戏异常监视。'
    Assert-NotCancelled
    if ($targetProcess.HasExited) { throw '游戏已在开始采集前退出；下次启动游戏时会重新监视。' }
    $stdoutPath = Join-Path $sessionPath 'procdump.log'
    $stderrPath = Join-Path $sessionPath 'procdump-error.log'
    $script:captureProcess = Start-Process -FilePath $ProcDumpPath -ArgumentList $arguments -PassThru -WindowStyle Hidden -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath
    $null = $captureProcess.Handle # PowerShell 5.1 retains this handle to read ExitCode.
    $shownLines = 0
    $armed = $false
    while ($true) {
        Assert-NotCancelled
        # ProcDump writes UTF-16LE without a BOM when stdout is redirected.
        $currentOutput = Get-Content -LiteralPath $stdoutPath -Raw -Encoding Unicode -ErrorAction SilentlyContinue
        $lines = @()
        if ($currentOutput -and $currentOutput.LastIndexOf("`n") -ge 0) {
            $lines = @($currentOutput.Substring(0, $currentOutput.LastIndexOf("`n")).Split("`n"))
        }
        while ($shownLines -lt $lines.Count) {
            $line = $lines[$shownLines++]
            Write-Host $line
            if (-not $armed -and $line -match 'Press Ctrl-C to end monitoring') {
                $armed = $true
                $metadata.status = 'monitoring'
                $metadata.monitoring_at = [DateTimeOffset]::Now.ToString('o')
                Save-Metadata
                Publish-Status 'monitoring' '正在监视游戏异常。'
                if (-not $Managed) {
                    Write-Host '已开始监视异常。请照常联机；保留此窗口。停止采集请按 Ctrl+C，不要直接关窗口。' -ForegroundColor Green
                }
            }
        }
        if ($captureProcess.HasExited) { break }
        Wait-Cancellable 500
    }
    $captureProcess.WaitForExit()
    $metadata.procdump_exit_code = $captureProcess.ExitCode
    $captureLog = Get-Content -LiteralPath $stdoutPath -Raw -Encoding Unicode
    $errorText = Get-Content -LiteralPath $stderrPath -Raw -ErrorAction SilentlyContinue
    if ($errorText) { Write-Host $errorText }
    $dumps = @(Get-ChildItem -LiteralPath $sessionPath -Filter '*.dmp' -File)
    $metadata.dumps = @($dumps | ForEach-Object { Read-DumpInfo $_.FullName })
    if ($dumps.Count -gt 0) {
        if ($captureLog -notmatch 'Dump 1 complete:') { throw '没有找到转储写入完成记录，请保留文件供检查。' }
        foreach ($dump in $metadata.dumps) {
            if (-not $dump.full_memory -or -not $dump.exception_code) { throw '转储缺少完整内存标志或异常信息，请保留文件供检查。' }
        }
        $metadata.status = 'exception_captured'
        Write-Host '已捕获异常并生成完整内存转储。异常可能被游戏自行处理，不代表游戏一定会退出。' -ForegroundColor Green
        Publish-Status 'monitoring' '异常现场已写入，正在补收日志。'
        Wait-Cancellable 5000
    } elseif ($armed -and $targetProcess.HasExited -and $captureProcess.ExitCode -eq 0) {
        $metadata.status = 'process_exited_without_matching_exception'
        Write-Host '游戏已退出，本次没有捕获到匹配的异常。不能仅凭此结果排除崩溃。'
    } else {
        throw '监视已结束，但没有取得异常转储，也未确认游戏退出，请查看 procdump.log。'
    }
    Finish-CaptureSession
}

try {
    if ($Managed) {
        if (-not $ControlDirectory -or -not (Test-Path -LiteralPath $ControlDirectory -PathType Container)) {
            throw '自动记录缺少客户端创建的控制目录。'
        }
        $ControlDirectory = (Resolve-Path -LiteralPath $ControlDirectory).ProviderPath
        Publish-Status 'preparing' '正在准备自动记录闪退。'
        if (-not $AcceptEula) { throw '自动记录必须先在客户端确认微软 ProcDump 许可。' }
        if ($ParentPid -le 0 -or $ParentCreated -le 0) { throw '自动记录缺少客户端进程标识。' }
        $parentProcess = [Diagnostics.Process]::GetProcessById($ParentPid)
        $null = $parentProcess.Handle
        if ($parentProcess.HasExited -or $parentProcess.StartTime.ToFileTimeUtc() -ne $ParentCreated) {
            throw '客户端进程标识已失效，自动记录未启动。'
        }
        Assert-NotCancelled
    } else {
        Write-Host 'NetBurrow 闪退采集工具'
        Write-Host '只监视本次游戏进程；捕获一份完整内存后结束。文件只保存在本机。'
        Write-Host '完整内存可能很大并含私人数据。请预留数 GB 空间，采集时游戏可能短暂停顿。'
    }
    if (-not $GamePath) {
        $settingsPath = Join-Path $env:LOCALAPPDATA 'NetBurrow\settings.json'
        if (Test-Path -LiteralPath $settingsPath) {
            $GamePath = (Get-Content -LiteralPath $settingsPath -Raw -Encoding UTF8 | ConvertFrom-Json).game_path
        }
    }
    if (-not $GamePath) { throw '没有找到游戏路径。请先在 NetBurrow 中保存游戏路径，或通过 -GamePath 指定 isaac-ng.exe。' }
    $GamePath = (Resolve-Path -LiteralPath $GamePath).ProviderPath
    if ([IO.Path]::GetFileName($GamePath) -ine 'isaac-ng.exe' -or -not (Test-Path -LiteralPath $GamePath -PathType Leaf)) {
        throw '游戏路径必须指向 isaac-ng.exe。'
    }
    if (-not $AcceptEula) {
        Write-Host '采集使用微软 ProcDump：https://learn.microsoft.com/sysinternals/downloads/procdump'
        Write-Host '许可条款：https://learn.microsoft.com/sysinternals/license-terms'
        if ((Read-Host '同意该工具许可并开始采集？输入 Y 继续') -ine 'Y') { throw '未接受 ProcDump 许可，采集未启动。' }
    }
    if (-not $ProcDumpPath) {
        $toolDirectory = Join-Path $env:LOCALAPPDATA 'NetBurrow\tools\ProcDump'
        $ProcDumpPath = Join-Path $toolDirectory 'procdump.exe'
        if (-not (Test-Path -LiteralPath $ProcDumpPath -PathType Leaf)) {
            Assert-NotCancelled
            New-Item -ItemType Directory -Force -Path $toolDirectory | Out-Null
            $archivePath = Join-Path $toolDirectory 'Procdump.zip'
            Write-Host '正在从微软官网下载 ProcDump（首次使用需要联网）……'
            Publish-Status 'preparing' '首次使用，正在从微软官网下载采集工具。'
            [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
            Invoke-WebRequest -UseBasicParsing -Uri 'https://download.sysinternals.com/files/Procdump.zip' -OutFile $archivePath -TimeoutSec 30
            Assert-NotCancelled
            Expand-Archive -LiteralPath $archivePath -DestinationPath $toolDirectory -Force
        }
    }
    Assert-NotCancelled
    $ProcDumpPath = (Resolve-Path -LiteralPath $ProcDumpPath).ProviderPath
    Assert-MicrosoftProcDump $ProcDumpPath
    $OutputRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputRoot)
    if (-not $Managed) { New-CaptureSession }
    Write-Host "等待游戏：$GamePath"
    while ($true) {
        Publish-Status 'waiting' '等待游戏启动；重新打开游戏后会自动继续监视。'
        while (-not $targetProcess) {
            Assert-NotCancelled
            try {
                $targetProcess = Find-GameProcess
                if (-not $targetProcess) { Publish-Status 'waiting' '等待游戏启动；重新打开游戏后会自动继续监视。' }
            } catch {
                if (-not $Managed) { throw }
                Publish-Status 'failed' $_.Exception.Message
            }
            if (-not $targetProcess) { Wait-Cancellable 500 }
        }
        try {
            Assert-NotCancelled
            if ($Managed) { New-CaptureSession }
            Capture-GameInstance
            $resultCode = 0
        } catch {
            if ($_.Exception -is [OperationCanceledException]) { throw }
            $metadata.status = 'failed'
            $metadata.error = $_.Exception.Message
            Write-Host "采集未成功：$($_.Exception.Message)" -ForegroundColor Red
            Stop-CaptureProcess
            if ($sessionPath -and -not $sessionComplete) { Finish-CaptureSession }
            Publish-Status 'failed' $_.Exception.Message
            if (-not $Managed) { throw }
        } finally {
            Stop-CaptureProcess
            if ($captureProcess) { $captureProcess.Dispose(); $captureProcess = $null }
        }
        if (-not $Managed) { break }
        # Success and failure both consume this process instance. Do not reattach
        # after a handled first-chance exception; wait for a genuinely new process.
        while (-not $targetProcess.HasExited) { Wait-Cancellable 500 }
        $targetProcess.Dispose()
        $targetProcess = $null
        $sessionPath = $null
        $sessionComplete = $false
    }
} catch {
    if ($_.Exception -is [OperationCanceledException]) {
        $cancelRequested = $true
        $metadata.status = 'cancelled'
        $resultCode = 0
    } else {
        if (-not $sessionComplete) {
            $metadata.status = 'failed'
            $metadata.error = $_.Exception.Message
        }
        Write-Host "采集未成功：$($_.Exception.Message)" -ForegroundColor Red
        Publish-Status 'failed' $_.Exception.Message
        $resultCode = 1
    }
} finally {
    $wasMonitoring = $captureProcess -and -not $captureProcess.HasExited
    Stop-CaptureProcess
    if ($sessionPath -and -not $sessionComplete) {
        if ($cancelRequested -or $wasMonitoring) { $metadata.status = 'cancelled' }
        if ($targetProcess -and $targetProcess.HasExited) { $metadata.game_exit_code = '0x{0:X8}' -f $targetProcess.ExitCode }
        if (-not $Managed) { Copy-Logs 'after' }
        $metadata.finished_at = [DateTimeOffset]::Now.ToString('o')
        # Cancelled/incomplete sessions retain evidence but can never be bundled.
        Save-Metadata
    }
    if ($captureProcess) { $captureProcess.Dispose() }
    if ($targetProcess) { $targetProcess.Dispose() }
    if ($parentProcess) { $parentProcess.Dispose() }
    if ($Managed -and $cancelRequested) { Publish-Status 'stopped' '自动记录已停止，异常监视已安全解除。' }
    if (-not $Managed -and $sessionPath) {
        Write-Host '若有 .dmp，请把本次整个文件夹压缩后私下发送给排查者；重开游戏需要重新运行脚本。'
    }
}
exit $resultCode
