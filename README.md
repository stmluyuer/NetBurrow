# NetBurrow

给《以撒的结合：忏悔+》用的 Windows 联机工具，通过 Relay 服务器中转游戏数据。

[下载客户端](https://github.com/stmluyuer/NetBurrow-Releases) · [更新日志](CHANGELOG.md) · [部署服务器](docs/server-ai-handoff.md) · [项目架构](docs/architecture.md)

## 开始使用

需要 **64 位 Windows** 和 **32 位 `isaac-ng.exe`**。同一局的朋友都要开启 NetBurrow，使用相同的服务器地址和完整组码。

1. 向管理员取得 Relay 地址和已授权的组码。自建服务器请先看[部署说明](docs/server-ai-handoff.md)；地址格式为 `主机名或IP:端口`，默认端口是 `24872`，TCP 和 UDP 共用该端口。
2. 下载并完整解压 `NetBurrow-<版本>-win-x64.zip`。把 `NetBurrow.exe`、`netburrow-injector.exe`、`netburrow_hook.dll` 留在同一文件夹，保留随包的许可文件，不要混用不同版本。
3. 打开 `NetBurrow.exe`，填写服务器地址和组码，确认游戏路径指向实际的 `isaac-ng.exe`。传输方式先用默认的 TCP。
4. 点击“连接”，出现“等待游戏”后，从 Steam 启动游戏，邀请朋友并开局。

“新建组”只会在本机生成组码。新组码须由管理员加入服务器白名单，并在没有对局时重启 Relay 后才能使用。组码相当于入组凭据，请只私下分享给同组朋友；它不验证 Steam 账号，Relay 通信也没有 TLS 加密。

## 使用时要知道

- 游戏已经打开时，可开启“接入已运行的游戏（实验性）”。这个选项默认关闭，只能在尚未联机的主菜单使用；接入失败后要重开游戏。
- 关闭窗口默认会退出并断开。想让工具继续运行，在设置中选择“最小化并保持连接”。
- 断线时先等工具恢复。支持会话恢复的 Relay 最长会尝试约 120 秒；恢复后游戏仍卡住、恢复失败、主动断开或工具异常退出时，都需要完全退出游戏再重新连接。
- 双方客户端和 Relay 都支持 UDP 时，可以选择“UDP 优先”，可靠消息仍走 TCP。
- 成员列表显示各人到 Relay 的往返延迟。

在“关于”页检查更新，也可开启默认关闭的“启动时检查更新”。下载后先退出游戏和工具，把新版完整解压到新文件夹，本机设置会保留。服务器更新由管理员在当前对局结束后处理，步骤见[部署说明](docs/server-ai-handoff.md)。

## 遇到问题

- **连不上或看不到朋友：** 按界面提示处理，先核对服务器地址、完整组码及管理员授权。接入游戏失败时，检查路径和工具文件，重开游戏后再试。
- **画面卡住但连接还在：** 在“日志与诊断”点击“记录卡住现场”，并记下其他玩家是否也卡住。
- **需要反馈问题：** 点击“导出诊断”生成简要报告，或用“导出日志包”整理日志和最近一次完整崩溃记录。文件保存在 `%LOCALAPPDATA%\NetBurrow\diagnostics`，不会自动上传；请附上发生时间、操作步骤和工具版本，不要发送 `settings.json`。
- **偶发闪退：** 可开启默认关闭的“自动记录闪退”，按提示确认微软 ProcDump 许可。完整转储可能占用数 GB 并含个人信息，只分享给可信的排查人员。详见[闪退采集说明](docs/crash-capture.md)。

普通日志位于 `%LOCALAPPDATA%\NetBurrow\logs`，设置位于 `%LOCALAPPDATA%\NetBurrow\settings.json`。

## 开发者构建与测试

Windows 构建需要 Rust MSVC 工具链，以及 Visual Studio Build Tools 的“使用 C++ 的桌面开发”（含 x86/x64 工具和 Windows SDK）。Rust 需支持 2024 edition，并满足锁定依赖的要求。首次构建需要联网下载依赖。

在仓库根目录的 PowerShell 中执行：

```powershell
rustup update stable
rustup default stable
rustup target add x86_64-pc-windows-msvc i686-pc-windows-msvc
cargo fetch --locked
cargo build --locked --target x86_64-pc-windows-msvc -p netburrow-app
cargo build --locked --target i686-pc-windows-msvc -p netburrow-injector -p netburrow-hook
```

按改动选择相关检查：

```powershell
cargo test --locked -p netburrow-protocol -p netburrow-relay -p netburrow-core -p netburrow-app
cargo test --locked -p netburrow-hook
# Windows x86 注入、Hook 与 IPC 检查，使用模拟游戏进程
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\test-native.ps1
```

实现和测试范围见[架构说明](docs/architecture.md)。

## 开发者打包

右键运行根目录的 `打包客户端.ps1`，或执行：

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\打包客户端.ps1
# 同时生成客户端包和 Relay 源码包
powershell -ExecutionPolicy Bypass -File .\scripts\package.ps1
```

产物在 `.local\dist`。默认将版本末位加 1；可用 `-Version` 指定版本，重复打包或失败后重试用 `-KeepVersion`。保留版本会覆盖同版本 ZIP，已经分发的包有变化时应使用新版本。工具链和依赖缓存齐全时，`scripts\package.ps1` 可加 `-Offline`。

打包需要根目录的 `LICENSE` 和 `THIRD_PARTY_LICENSES.txt`，这两个文件也会放进 ZIP。

<details>
<summary>发布客户端更新</summary>

客户端从独立的 [NetBurrow-Releases](https://github.com/stmluyuer/NetBurrow-Releases) 仓库检查更新。准备非空的 UTF-8 更新说明（最多 12000 个字符），执行：

```powershell
.\打包客户端.ps1 -ReleaseNotesPath .\更新说明.txt
```

在发行仓库创建 `v<版本>` 的 Draft Release，上传脚本提示的客户端 ZIP 和 `.local\dist\updates\<版本>\latest.json`。核对标签、清单和程序版本后再发布并标记 Latest，注明对应的源码提交；不要把测试包设为最新正式版。

发布后，确认未登录也能下载[更新清单](https://github.com/stmluyuer/NetBurrow-Releases/releases/latest/download/latest.json)和 ZIP，再用旧客户端检查。若需要同步更新 Relay，须在更新说明中写明。当前客户端不校验 ZIP 签名，也不检查 Relay 协议兼容性；分支项目自行发行时需要修改客户端更新地址。

</details>

## 许可与参考

NetBurrow 自有代码采用 [MIT 许可证](LICENSE)。游戏接入策略参考 [TractorBeam](https://github.com/mcthesw/TractorBeam/tree/ea0393f5665dd1e0775d9dc1d0c987304461191c)，来源与许可边界见[架构说明](docs/architecture.md#实现来源与许可)。第三方依赖、字体和资产保留各自许可，见[第三方许可材料](THIRD_PARTY_LICENSES.txt)，再分发时请一并保留。

更新依赖后，可用 Python 3.11+ 运行 `python scripts/generate-third-party-licenses.py` 更新许可清单（需已有 Cargo 源码缓存及网络）。清单覆盖全部锁定依赖；其中 `dispatch 0.2.0` 缺少上游许可全文，会使生成器报告非零退出码。它不在当前 Windows 客户端、x86 接入组件或 Linux Relay 的依赖图中，变更发布目标时需重新核对。
