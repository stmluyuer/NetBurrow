# NetBurrow Relay 服务器交接

这份说明交给 VPS 上的 AI 执行。它只更新 Relay 源码和既有服务，不部署数据库、不开放新端口、不改客户端配置，也不访问用户未授权的机器。所有命令完成后应把实际分支、commit、构建结果、服务状态和回滚点回报给用户。

## 固定来源和更新原则

2026-09-16 的序号诊断和队友探测需要新版 Relay 配合。扩展通过保留 Ping 值协商；旧客户端仍走原协议，新客户端连接旧 Relay 时继续通信但不启用两项诊断。部署无需新增端口、配置项或数据迁移。完整诊断需要双方客户端均更新；TCP 探测成功不代表游戏正在推进。此说明不代表当前服务器已更新，部署仍须使用用户指定的固定来源。

GitHub 仓库是 `https://github.com/stmluyuer/NetBurrow.git`，当前发布分支是 `main`。服务器端不得用浮动的 `HEAD`、默认分支或未确认的本地改动构建。

先以普通部署用户取得指定分支，并把远端引用解析为固定 commit。Git 的显式 refspec 会把远端分支写到指定远端跟踪引用，`git fetch` 的行为见 [Git 官方文档](https://git-scm.com/docs/git-fetch)。

```bash
REPO=https://github.com/stmluyuer/NetBurrow.git
BRANCH=main
SOURCE=/opt/netburrow/source

set -euo pipefail
if [ ! -d "$SOURCE/.git" ]; then
  git clone --branch "$BRANCH" --single-branch "$REPO" "$SOURCE"
fi
git -C "$SOURCE" fetch --no-tags origin "refs/heads/$BRANCH:refs/remotes/origin/$BRANCH"
COMMIT=$(git -C "$SOURCE" rev-parse "refs/remotes/origin/$BRANCH")
git -C "$SOURCE" cat-file -e "$COMMIT^{commit}"
git -C "$SOURCE" show --no-patch --format='commit=%H%nsubject=%s' "$COMMIT"
```

后续步骤只使用输出的 `$COMMIT`。如已有工作树，创建一个脱离分支的临时工作树；不要在正在服务的目录执行 `git pull`。

```bash
STAGE="/opt/netburrow/stage-$COMMIT"
git -C "$SOURCE" worktree add --detach "$STAGE" "$COMMIT"
```

`NetBurrow-<版本>-relay-source.zip` 仍可用于离线交付；源码 ZIP 中已经裁剪为 Relay 和协议 workspace。GitHub 更新优先使用完整 workspace，因为下方检查需要在完整 workspace 中运行。使用 ZIP 时把解压根目录设为 `$STAGE`，仍显式使用 `TARGET_DIR="$STAGE/.local/target"` 和相同的 `cargo +1.97.0 ... --target-dir "$TARGET_DIR"` 命令。

## 先核查现有服务，再构建验证

不要凭本文示例重建或覆盖现有 unit。先核查实际服务名、运行账户、工作目录、`ExecStart`、TCP/UDP 端口和当前二进制路径，保留这些既有布局：

```bash
sudo systemctl cat netburrow-relay.service
sudo systemctl show netburrow-relay.service \
  -p User -p Group -p WorkingDirectory -p ExecStart \
  -p StandardOutput -p StandardError -p SyslogIdentifier --no-pager
sudo systemctl status netburrow-relay.service --no-pager
sudo ss -ltnup | grep ':24872' || true
```

在未停止服务前，以该 unit 的普通 `User`/`Group` 在 `$STAGE` 内构建和测试。根目录的 `rust-toolchain.toml` 包含 Windows target；Linux Relay 不安装这些 target，使用显式 `+1.97.0` 覆盖目录工具链选择。若该普通用户未安装该工具链，先安装最小 profile：

```bash
set -euo pipefail
if ! rustup toolchain list | awk '{print $1}' | grep -qx '1.97.0-x86_64-unknown-linux-gnu'; then
  rustup toolchain install 1.97.0 --profile minimal
fi

cd "$STAGE"
TARGET_DIR="$STAGE/.local/target"
RELAY_BIND=0.0.0.0:24872       # 以现有 ExecStart 的 --bind 为准
RELAY_MAX_CLIENTS=1024         # 以现有 ExecStart 的 --max-clients 为准
RELAY_ALLOWED_GROUPS=/etc/netburrow/allowed-groups.txt  # 以现有 --allowed-groups-file 为准
cargo +1.97.0 test -p netburrow-protocol -p netburrow-relay --locked --target-dir "$TARGET_DIR"
cargo +1.97.0 build --release -p netburrow-relay --locked --target-dir "$TARGET_DIR"
"$TARGET_DIR/release/netburrow-relay" --check-config --bind "$RELAY_BIND" --max-clients "$RELAY_MAX_CLIENTS" --allowed-groups-file "$RELAY_ALLOWED_GROUPS"
```

`--locked` 会要求现有 `Cargo.lock` 不发生依赖解析变更，详见 [Cargo build 官方文档](https://doc.rust-lang.org/cargo/commands/cargo-build.html)。`--target-dir` 必须显式指定，因为完整仓库 `.cargo/config.toml` 把产物定向到 `.local/target`；源码 ZIP 也使用同样的显式 target 目录。命令在完整 Linux workspace 中执行，不需要安装 `i686-pc-windows-msvc` 或 `x86_64-pc-windows-msvc` target。构建或检查失败时删除临时工作树并保留旧服务，不停服。

组白名单在启动时从现有 `--allowed-groups-file` 加载，不输出组码。参数缺失、文件不可读、内容无效或名单为空时拒绝启动；未授权的 Join 在分配成员编号和 UDP 凭据前被拒绝。白名单在进程内不变，Resume 只能凭恢复凭据恢复本进程中已获准的会话，不能创建新成员。保留现有名单和文件权限，不把真实组码提交到仓库。

## 经验证后切换和回滚

只有上述构建、测试和 `--check-config` 都通过后，才安排一局结束后的短暂停服。把下列变量替换为核查得到的真实值；保持端口、账户、unit 名称、参数和工作目录不变。

```bash
set -euo pipefail
SERVICE=netburrow-relay.service
SERVICE_USER=netburrow                  # 以 systemctl show 的 User 为准
SERVICE_GROUP=netburrow                 # 以 systemctl show 的 Group 为准
CURRENT_BINARY=/opt/netburrow/target/release/netburrow-relay  # 以 ExecStart 实际路径为准
NEW_BINARY="$STAGE/.local/target/release/netburrow-relay"
BACKUP_DIR=/opt/netburrow/backups

sudo install -d -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$BACKUP_DIR"
sudo cp --preserve=mode,timestamps "$CURRENT_BINARY" "$BACKUP_DIR/netburrow-relay.$(date +%Y%m%d-%H%M%S).$COMMIT.previous"
sudo systemctl stop "$SERVICE"
sudo install -o "$SERVICE_USER" -g "$SERVICE_GROUP" -m 0755 "$NEW_BINARY" "$CURRENT_BINARY"
sudo systemctl start "$SERVICE"
sudo systemctl status "$SERVICE" --no-pager
```

如果启动、监听或小范围连接检查失败，停止 service，把刚才的 `.previous` 备份复制回 `$CURRENT_BINARY`，再启动 service；不要删除旧二进制或修改全局 systemd/journald 设置。成功后也保留旧 binary 和 `$COMMIT`，供下一次回滚使用。

Relay 默认使用 `0.0.0.0:24872`，同一端口需要 TCP 与 UDP。仅在既有 VPS 防火墙和云安全组中核查这两条规则；本更新不应新增管理端口。Relay 组码不是完整认证体系，也没有 TLS，不应作为公开匹配服务。

## 成员状态与兼容性

本版本增加同组成员状态同步。客户端约每 3 秒上报可选显示名、游戏阶段、到 Relay 的 RTT、实际传输状态和累计收发数；Relay 按真实连接归属给同组已订阅客户端发送快照，成员断开或重启游戏时清除旧统计。RTT 不是玩家之间的端到端延迟，状态不落盘。

先更新 Relay，再让朋友更新客户端。新版客户端连接旧 Relay 会在接入游戏前提示更新服务器；新版 Relay 仍接受旧客户端，但不向未上报状态的旧客户端发送扩展状态消息。

## Relay 日志和 journal

### 原会话恢复兼容性

客户端通过保留的 `Ping(0x4e425253554d0001)` 协商恢复能力。新版 Relay 对意外断开的已协商会话保留原成员与游戏绑定约 120 秒，恢复握手核对原连接编号、随机恢复凭据和可靠接收水位。可靠消息保留至确认，重发记录仍计入单连接和全局队列字节预算；过期、明确退出或协议错误会清理。旧客户端保持原断线行为，旧 Relay 不提供此功能。

状态仅存内存，服务重启不能恢复旧局，因此仍应在对局结束后更新服务器。客户端需成套更新 helper/Hook。恢复后该客户端本次启用固定走 TCP。无需新增端口、依赖、配置或磁盘状态。本地回环和 x86 宿主测试不能代替公网四人续局验证。

Relay 把 stderr 交给 systemd journal。在前述停服切换之前核查 `StandardOutput`、`StandardError` 和 `SyslogIdentifier`；只有 stderr 尚未交给 journal 时，才创建本服务的 drop-in，随该次服务重启生效：

```bash
sudo systemctl edit netburrow-relay.service
# 写入：
# [Service]
# StandardOutput=journal
# StandardError=journal
sudo systemctl daemon-reload
```

不改 unit 的其他既有字段，也不改全局 journald 配置。每条日志带 `unix_ms` 时间，字段和语义如下：

- 生命周期事件包括 `started`、`summary`、`joined`、`game_binding`、`read_closed`、`disconnected`、`session_detached`、`session_resumed`、`stopped`，以及对应的收发错误事件。`session_detached` 表示仍在保留窗口，`session_resumed` 表示恢复了原 Relay 会话，不代表游戏同步已恢复。
- `event=summary` 每 10 秒一次。`online`、`game_bound`、`udp_bound`、`queued_bytes` 是当时快照；`accepted`、`joined`、`disconnected`、`handshake_rejected`、`capacity_rejected`、`tcp_data_received`、`tcp_data_written`、`udp_data_received`、`udp_data_sent`、`udp_invalid`、`udp_bind_rejected`、`protocol_rejected`、`queue_failed`、`io_failed` 是进程启动后的累计值。
- `event=stopped` 记录停服时快照。`tcp_data_written` 只表示 `write_all` 成功，不表示对端游戏已收到；`udp_data_sent` 只表示系统 `send_to` 成功，不表示 UDP 已送达。`protocol_rejected` 表示因协议或慢队列等问题尝试向客户端返回错误的次数；错误回复也可能因连接关闭或队列满而未发出。
- 不逐包刷日志；不得写入组码、token、显示名、SteamID、IP 地址或 payload。

查看最近记录、持续跟随和导出一个时间段：

```bash
sudo journalctl -u netburrow-relay.service -n 100 --no-pager
sudo journalctl -u netburrow-relay.service -f
sudo journalctl -u netburrow-relay.service --since '2026-09-14 00:00:00' --until now --output=short-full --no-pager > relay-journal.txt
```

`journalctl -u` 按 unit 过滤，`--since`、`--until` 用于时间范围；语义见 [systemd 官方 journalctl 文档](https://www.freedesktop.org/software/systemd/man/latest/journalctl.html)。只核查现有保留策略和磁盘占用：

```bash
sudo journalctl --disk-usage
sudo systemd-analyze cat-config systemd/journald.conf
```

不要为本服务修改全局 `journald.conf`、清空 journal 或改变系统保留限制。若现有策略不足，先向用户报告观察到的限制和所需空间，由用户决定。

## 验收边界

向用户报告固定 `$COMMIT`、Relay 测试和构建结果、旧 binary 备份路径、service 状态、现有 TCP/UDP 规则和 journal 核查结果。然后由两台获授权 Windows 机器测试同组连接、正常 Steam 邀请、开局、过房间、退出以及 UDP 优先收发。不要把 Linux 构建、回环测试或 service 运行描述为真实游戏已验证。
