# NetBurrow Relay 部署与更新

本说明供服务器管理员首次部署或更新 Relay。执行者只操作已获授权的机器；首次部署需要创建运行账户、服务和 TCP/UDP 规则，更新已有服务时保留其账户、端口、路径和配置。部署不需要数据库。记录来源、构建结果、服务状态及更新时的回滚点。

## 取得固定来源

以下两种来源选一种。使用普通部署账户准备源码；不要在正在服务的目录执行 `git pull`。Linux 构建需要 Git（仅 Git 来源）、支持 Rust 2024 edition 及锁定依赖的 Rust 工具链、C 编译器和链接器。使用源码 ZIP 还需 unzip、sha256sum。先用 `rustc --version` 和 `cargo --version` 确认环境；首次构建需要获取依赖，源码 ZIP 本身不含离线依赖缓存。

### 从 Git 取得源码

使用发布者指定的分支或标签对应的固定提交。以下示例取得 `main` 的当前提交并锁定到独立目录；部署前核对打印出的提交确为本次要部署的版本。

```bash
set -euo pipefail
REPO=https://github.com/stmluyuer/NetBurrow.git
BRANCH=main
SOURCE="$HOME/netburrow-source"
if [ ! -d "$SOURCE/.git" ]; then
  git clone --branch "$BRANCH" --single-branch "$REPO" "$SOURCE"
fi
git -C "$SOURCE" fetch --no-tags origin "refs/heads/$BRANCH:refs/remotes/origin/$BRANCH"
COMMIT=$(git -C "$SOURCE" rev-parse "refs/remotes/origin/$BRANCH")
git -C "$SOURCE" cat-file -e "$COMMIT^{commit}"
git -C "$SOURCE" show --no-patch --format='commit=%H%nsubject=%s' "$COMMIT"
SOURCE_ID="$COMMIT"
STAGE="$HOME/netburrow-stage-$SOURCE_ID"
git -C "$SOURCE" worktree add --detach "$STAGE" "$COMMIT"
```

后续只使用该 `$STAGE` 和 `$SOURCE_ID`。重试时可复用同一干净工作树，不要覆盖已有改动。

### 从 Relay 源码 ZIP 取得源码

`NetBurrow-<版本>-relay-source.zip` 只包含 Relay 和协议 workspace，不依赖 Git。把 `ZIP` 改为收到的文件路径；包的 SHA-256 作为此次来源标识，后续备份不再依赖 Git 变量。

```bash
set -euo pipefail
ZIP="$HOME/NetBurrow-<版本>-relay-source.zip"
ZIP_HASH=$(sha256sum "$ZIP" | cut -d ' ' -f 1)
SOURCE_ID="zip-$ZIP_HASH"
STAGE="$HOME/netburrow-stage-$SOURCE_ID"
mkdir "$STAGE"
unzip "$ZIP" -d "$STAGE"
printf 'source=%s\n' "$SOURCE_ID"
```

应通过可信渠道取得 ZIP；自行计算的哈希只标识该文件，并不证明发布者身份。离线构建还需提前在兼容 Linux 环境准备 Cargo 依赖缓存，再给后续 Cargo 命令添加 `--offline`。项目与第三方许可材料位于源码仓库及 ZIP 根目录，再分发时请一并保留。

## 首次部署：账户和组白名单

已有服务请跳到下一节。以下首次部署示例适用于使用 systemd 的 Linux，并以 `netburrow-relay.service`、端口 `24872` 为例。先确认这些路径和名称尚未用于现有服务；不要将示例直接覆盖到已有 unit 上。

```bash
set -euo pipefail
if systemctl cat netburrow-relay.service >/dev/null 2>&1; then
  echo 'Service already exists; use the update procedure.' >&2
  exit 1
fi
if ! id netburrow >/dev/null 2>&1; then
  sudo useradd --system --user-group --no-create-home --shell /usr/sbin/nologin netburrow
fi
sudo install -d -o root -g netburrow -m 0750 /etc/netburrow
sudo install -d -o root -g root -m 0755 /opt/netburrow/bin
sudoedit /etc/netburrow/allowed-groups.txt
sudo chown root:netburrow /etc/netburrow/allowed-groups.txt
sudo chmod 0640 /etc/netburrow/allowed-groups.txt
```

在 `sudoedit` 中填入实际授权组码，每行一个 `NB1-` 加 64 位十六进制字符，允许空行和 `#` 开头的注释。由客户端“新建组”生成组码，再由管理员登记；不要照抄示例凭据或把真实组码提交到仓库。名单不能为空。将已登记组码私下交给参与者，客户端填写服务器的 `主机名或IP:24872`。

## 核查配置并构建

更新已有服务时，先核对实际服务名、运行账户、工作目录、`ExecStart`、TCP/UDP 端口、白名单与当前二进制路径：

```bash
sudo systemctl cat netburrow-relay.service
sudo systemctl show netburrow-relay.service \
  -p User -p Group -p WorkingDirectory -p ExecStart \
  -p StandardOutput -p StandardError -p SyslogIdentifier --no-pager
sudo systemctl status netburrow-relay.service --no-pager
sudo ss -ltnup | grep ':24872' || true
```

首次部署使用下方示例值；更新时按现有配置填写。源码的构建和测试由普通部署账户完成，不停止服务；配置检查用服务账户，确认实际读取权限。Linux Relay 无需安装 Windows target。

```bash
set -euo pipefail
cd "$STAGE"
TARGET_DIR="$STAGE/.local/target"
SERVICE_USER=netburrow
RELAY_BIND=0.0.0.0:24872
RELAY_MAX_CLIENTS=1024
RELAY_ALLOWED_GROUPS=/etc/netburrow/allowed-groups.txt
cargo test -p netburrow-protocol -p netburrow-relay --locked --target-dir "$TARGET_DIR"
cargo build --release -p netburrow-relay --locked --target-dir "$TARGET_DIR"
# The service account may not traverse the deploy user's home directory.
# Check with a temporary executable accessible to that account.
CHECK_BINARY=$(mktemp /tmp/netburrow-check.XXXXXX)
trap 'rm -f "$CHECK_BINARY"' EXIT
install -m 0755 "$TARGET_DIR/release/netburrow-relay" "$CHECK_BINARY"
sudo -u "$SERVICE_USER" "$CHECK_BINARY" --check-config --bind "$RELAY_BIND" --max-clients "$RELAY_MAX_CLIENTS" --allowed-groups-file "$RELAY_ALLOWED_GROUPS"
rm -f "$CHECK_BINARY"
trap - EXIT
```

`--locked` 要求不修改锁定依赖。显式 `--target-dir` 使完整 Git workspace 和裁剪 ZIP 使用相同产物路径。构建或检查失败时保留旧服务，不切换二进制。

组白名单仅在启动时加载，缺失、不可读、无效或为空时拒绝启动。新增组需修改名单，并在没有对局时重启 Relay；只修改文件不会立即生效。未授权 Join 在分配成员编号和 UDP 凭据前被拒绝。Resume 只能恢复本进程已获准的会话。

队列和可靠重放共同计入内存预算。默认总预算为 64 MiB，按白名单中的授权组数均分；同时保留每客户端 4 MiB 上限。不要堆积无用的授权组，否则每组可用预算会下降。组码不是 Steam 账号认证，连接没有 TLS；此工具不适合作为公开匹配服务。

## 首次部署：安装并启动

仅在上述测试、构建和配置检查通过后执行。下面 unit 对应首次部署示例值；修改地址、端口或白名单路径时，同步修改并重新运行配置检查。

```bash
set -euo pipefail
sudo install -o root -g root -m 0755 "$TARGET_DIR/release/netburrow-relay" /opt/netburrow/bin/netburrow-relay
# noclobber prevents replacing an existing unit file.
sudo sh -c 'set -C; cat > /etc/systemd/system/netburrow-relay.service' <<'UNIT'
[Unit]
Description=NetBurrow Relay
After=network.target

[Service]
User=netburrow
Group=netburrow
ExecStart=/opt/netburrow/bin/netburrow-relay --bind 0.0.0.0:24872 --max-clients 1024 --allowed-groups-file /etc/netburrow/allowed-groups.txt
Restart=on-failure
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
UNIT
sudo systemctl daemon-reload
sudo systemctl enable --now netburrow-relay.service
sudo systemctl status netburrow-relay.service --no-pager
sudo ss -ltnup | grep ':24872'
```

按服务器实际防火墙和云安全组放行所选端口的 TCP 与 UDP，范围限于需要接入的网络；不要增加其他管理端口。先检查端口监听，再进行小范围客户端连接测试。失败时查看本服务 journal，不要反复覆盖 unit。

## 更新已有服务：切换和回滚

仅在构建、测试和配置检查通过后，安排一局结束后的短暂停服。把下列变量改为核查得到的真实值，保持既有布局；`$SOURCE_ID` 对 Git 和 ZIP 路径都已定义。

```bash
set -euo pipefail
SERVICE=netburrow-relay.service
SERVICE_USER=netburrow
SERVICE_GROUP=netburrow
CURRENT_BINARY=/opt/netburrow/bin/netburrow-relay
NEW_BINARY="$STAGE/.local/target/release/netburrow-relay"
BACKUP_DIR=/opt/netburrow/backups
BACKUP_BINARY="$BACKUP_DIR/netburrow-relay.$(date +%Y%m%d-%H%M%S).$SOURCE_ID.previous"
sudo install -d -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$BACKUP_DIR"
sudo cp --preserve=mode,timestamps "$CURRENT_BINARY" "$BACKUP_BINARY"
printf 'backup=%s\n' "$BACKUP_BINARY"
sudo systemctl stop "$SERVICE"
sudo install -o "$SERVICE_USER" -g "$SERVICE_GROUP" -m 0755 "$NEW_BINARY" "$CURRENT_BINARY"
sudo systemctl start "$SERVICE"
sudo systemctl status "$SERVICE" --no-pager
```

如果启动、监听或小范围连接检查失败，停止 service，把记录的 `$BACKUP_BINARY` 复制回 `$CURRENT_BINARY`，再启动；不要删除旧二进制。成功后仍保留旧 binary 和来源标识。更新不应新增端口或修改全局 systemd/journald 设置。

## 成员状态与兼容性

本版本增加同组成员状态同步。客户端约每 3 秒上报可选显示名、游戏阶段、到 Relay 的 RTT、实际传输状态和累计收发数；Relay 按真实连接归属给同组已订阅客户端发送快照，成员断开或重启游戏时清除旧统计。RTT 不是玩家之间的端到端延迟，状态不落盘。

先更新 Relay，再让朋友更新客户端。新版客户端连接旧 Relay 会在接入游戏前提示更新服务器；新版 Relay 仍接受旧客户端，但不向未上报状态的旧客户端发送扩展状态消息。

## Relay 日志和 journal

### 原会话恢复兼容性

客户端通过保留的 `Ping(0x4e425253554d0001)` 协商恢复能力。新版 Relay 对意外断开的已协商会话保留原成员与游戏绑定约 120 秒，恢复握手核对原连接编号、随机恢复凭据和可靠接收水位。可靠消息保留至确认，重发记录仍计入单连接、组和全局队列字节预算；过期、明确退出或协议错误会清理。旧客户端保持原断线行为，旧 Relay 不提供此功能。

新版客户端遇到恢复拒绝仍在 120 秒总预算内重试。Relay 仅在原会话不存在时，以现有 Error 消息返回 `session resume unavailable; rejoin allowed`，明确允许客户端在新连接上重新 Join；Join 仍检查组白名单，Bind 仍检查游戏身份唯一性。仍存在但凭据错误或状态无效的会话不获得该许可；已过期会话先由原清理逻辑移除，随后才允许重新加入。旧客户端收到此错误仍按原逻辑结束恢复，旧 Relay 不返回许可时新版客户端只重试原会话。`session_resume_rejected` 日志记录拒绝类型和原因，不记录恢复凭据。

状态仅存内存，服务重启后只能尝试重新加入，不能恢复旧会话的可靠数据。客户端保留存活的 Hook、重新绑定同一游戏实例，丢弃旧 Relay 队列与重放记录，并重新协商恢复能力。不能保证游戏无损续局，因此仍应在对局结束后更新服务器。客户端需成套更新 helper/Hook。恢复后该客户端本次启用固定走 TCP。无需新增端口、依赖、配置或磁盘状态。本地回环和 x86 宿主测试不能代替公网四人续局验证。

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

报告固定 `$SOURCE_ID`、Relay 测试和构建结果、更新时的旧 binary 备份路径、service 状态、TCP/UDP 规则和 journal 核查结果。然后由两台获授权 Windows 机器测试同组连接、正常 Steam 邀请、开局、过房间、退出以及 UDP 优先收发。不要把 Linux 构建、回环测试或 service 运行描述为真实游戏已验证。
