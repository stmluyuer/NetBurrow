# 自动记录闪退

适用于 0.2.1 客户端。采集脚本已内置到 `NetBurrow.exe`，客户端 ZIP 只包含程序三件套，不再附带文档和手动采集入口。首次开启后，日常游玩无需打开采集窗口或手动整理现场。当前仍未确定偶发闪退的根因；“捕获到异常”也不等同于游戏一定已经闪退。

## 怎么用

1. 退出游戏和旧客户端，将新版完整解压，打开同一文件夹的 `NetBurrow.exe`，确认游戏路径。
2. 进入“日志与诊断”，开启 **自动记录闪退**。首次阅读采集说明和微软 ProcDump 许可后，点击“同意许可并开启”。选择会保留，以后开关不重复询问。
3. 照常启用联机、启动游戏。后台自动等待游戏、接入并显示“正在监视”；首次需要下载官方工具并验证微软签名。游戏正常退出或重开后会自动继续等待，无需再次开启。
4. 异常捕获完成后，工具自动整理一个 ZIP；在“日志与诊断”点击 **打开排查包**。启用了桌面通知时会收到完成提示，后台打包不会突然打开资源管理器。
5. 也可以点击原有 **一键打包日志**，重新生成包含最近一次完整崩溃现场和当前日志的 ZIP。转储正在写入时会跳过并说明，请等采集完成后再打包。

关闭开关、停止联机或退出客户端会请求正常解除监视。准备、下载、等待和日志收集期间也可取消。客户端意外退出时，后台会检测到并解除监视；若调试器解除较慢，会显示原因并继续等待，不强杀调试器或游戏。失败后可点击“重新监视”。

每个游戏实例最多采集一次异常，避免反复写入大文件。异常被游戏处理后仍可能继续运行，此时已完成的记录保持不变，监视程序等待下次重开。脚本不启动、关闭或重启游戏。

## 文件在哪里

- 原始记录：`%LOCALAPPDATA%\NetBurrow\crashes\日期时间-编号`。
- 排查 ZIP：`%LOCALAPPDATA%\NetBurrow\diagnostics\NetBurrow-logs-编号.zip`。
- ZIP 根目录有当前三个运行日志、`diagnostics.txt` 和 `crash-capture.txt`。最近一次完整现场放在 `crashes/<记录名>/`，包括 `.dmp`、`capture.json`、ProcDump 日志，以及 `before` / `after` 中的采集前后日志。
- `capture.complete` 表示转储已经验证、日志和元数据已经写完。它只在原始目录里使用，不作为分析文件加入 ZIP。未完成、正常退出、失败和取消的记录不会被当成崩溃包。

打包只加入最近一次完整记录，避免历史转储不断增大 ZIP；不读取 `settings.json` 或采集目录中的任意其他文件。选中记录无法读取时会明确报错。大转储直接写入支持大文件的 ZIP，无需先复制一份临时现场。旧版手动采集目录没有完成标记，需要继续按旧方式单独压缩，或使用新版重新采集。

## 能记录什么

- `*.dmp`：ProcDump `-ma` 生成的完整进程内存，包括异常、线程现场、模块和当时仍存在的游戏缓冲区。捕获首次通知的访问冲突 `0xC0000005`、快速失败 `0xC0000409`、堆损坏 `0xC0000374`，同时监视未处理异常。
- `capture.json`：接入时间、进程路径和启动时间、游戏及同目录客户端三件套的 SHA256、ProcDump 版本和返回码、转储大小/完整内存标志/异常码，以及采集结束时能取得的游戏退出码。不复制设置文件。
- `before` / `after`：NetBurrow 当前及上一份滚动日志，以及存在的以撒 Repentance+、Repentance、Rebirth `log.txt`。日志复制失败记入 `collection-warnings.txt`，不因此丢弃转储。
- `procdump.log` / `procdump-error.log`：接入、触发、写入和失败记录。

客户端也会在 `client.log` 记录游戏退出码。游戏自行处理异常后可能以正常码退出，不能仅凭零退出码排除崩溃。

## 使用边界

自动记录默认关闭。完整内存可能达到数 GB，请预留足够空间；其中可能包含联机身份、聊天或其他个人数据。原始记录和 ZIP **仅保存在本机，不自动上传**，请只分享给可信的排查人员。排查结束后可自行删除不再需要的记录和 ZIP。

应在异常发生前看到“正在监视”。采集会改变运行时序，写入完整内存时游戏可能短暂停顿。强制结束进程、断电、系统崩溃、空间不足或接入失败时可能拿不到转储；成功采集也不保证一次确定根因。

如果游戏以管理员身份运行，客户端也需要有调试该进程的权限；不要为采集关闭系统安全功能。已有其他调试器接入时，请先正常结束另一调试会话。支持系统以微软官方工具说明为准。

## 源码中的手动入口

源码仓库保留 `scripts\Capture-Crash.cmd` 和 `scripts\Capture-Crash.ps1`，供开发排查使用，不随客户端 ZIP 分发。自动记录关闭时，可双击源码中的 `Capture-Crash.cmd`，按提示确认许可，监视一次游戏。该方式每次只监视一个实例，不自动重启或生成 ZIP；默认目录中由新版脚本完成的记录，可以用客户端“一键打包日志”整理。提前停止请按 Ctrl+C 并等待解除监视。

需要指定路径、其他保存磁盘或离线工具时，在源码仓库根目录打开 PowerShell：

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\scripts\Capture-Crash.ps1 -GamePath "E:\SteamLibrary\steamapps\common\The Binding of Isaac Rebirth\isaac-ng.exe"
```

可加 `-PackageDirectory "D:\NetBurrow"` 指定客户端三件套所在目录，以便记录其版本与校验信息；加 `-OutputRoot "D:\IsaacCrashRecords"` 指定保存磁盘，或先从微软下载 ProcDump，再加 `-ProcDumpPath "D:\Tools\ProcDump\procdump.exe"`，脚本仍会验证签名。已阅读并同意许可时可加 `-AcceptEula`。自定义输出目录不参与客户端自动打包，请自行压缩该目录。

微软资料：[ProcDump 官方下载与用法](https://learn.microsoft.com/en-us/sysinternals/downloads/procdump)、[Sysinternals 软件许可](https://learn.microsoft.com/en-us/sysinternals/license-terms)。
