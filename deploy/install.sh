#!/bin/sh
# Avalon agent 一键安装 / 卸载。由 hub 下发，后台节点详情里有现成的命令。
#
#   安装：curl -fsSL https://hub.example.com/i/<钥匙> | sh -s -- --server https://hub.example.com --id 节点ID --token 节点Token
#         （完整命令在后台节点详情里；脚本地址里的钥匙由 token 派生，没有它拿不到脚本）
#   卸载：sh install.sh --uninstall，或者直接用后台给的那一行卸载命令
#
# 重复执行安装命令就是升级：换新程序、覆盖配置、重启服务。换了 token 也是重跑一次。
#
# 权限模型：安装需要 root（写 systemd 服务、建用户），agent 本身以专用的 probe-agent
# 用户运行——不能登录、没有家目录、没有任何特权能力，看不到 hub 的数据和自己的 token 文件。
set -eu

BIN=/usr/local/bin/probe-agent
CONF_DIR=/etc/linux-probe
ENV_FILE=$CONF_DIR/agent.env
UNIT=/etc/systemd/system/probe-agent.service
DROPIN_DIR=/etc/systemd/system/probe-agent.service.d
AGENT_USER=probe-agent

die() { printf '错误：%s\n' "$1" >&2; exit 1; }
say() { printf '%s\n' "$1"; }
# 只查本地 passwd 文件。不用 id：开了 nss-systemd 的机器上，id 会把 systemd 的临时用户也当真，
# 结果用户明明不存在，服务却起不来。
has_user() { grep -q "^$1:" /etc/passwd; }

SERVER=
ID=
TOKEN=
ACTION=install
while [ $# -gt 0 ]; do
    case "$1" in
        --server) [ $# -ge 2 ] || die "--server 缺少值"; SERVER=$2; shift 2 ;;
        --id) [ $# -ge 2 ] || die "--id 缺少值"; ID=$2; shift 2 ;;
        --token) [ $# -ge 2 ] || die "--token 缺少值"; TOKEN=$2; shift 2 ;;
        --uninstall) ACTION=uninstall; shift ;;
        *) die "未知参数：$1" ;;
    esac
done

[ "$(id -u)" = 0 ] || die "请用 root 执行（先 sudo -i）"
command -v systemctl >/dev/null 2>&1 || die "这台机器没有 systemd，请按文档手动安装"

if [ "$ACTION" = uninstall ]; then
    systemctl disable --now probe-agent >/dev/null 2>&1 || true
    rm -rf "$DROPIN_DIR"
    rm -f "$UNIT" "$BIN" "$BIN.new" "$ENV_FILE"
    rmdir "$CONF_DIR" 2>/dev/null || true
    systemctl daemon-reload
    if has_user "$AGENT_USER"; then userdel "$AGENT_USER" 2>/dev/null || true; fi
    say "已卸载 probe-agent。别忘了在后台把这个节点也删掉。"
    exit 0
fi

# 三个参数都会写进配置文件，格式不对就拒绝，绝不拿去拼别的命令。
case "$ID" in ''|*[!0-9a-f]*) die "--id 应为 16 位十六进制" ;; esac
[ ${#ID} -eq 16 ] || die "--id 应为 16 位十六进制"
case "$TOKEN" in ''|*[!0-9a-f]*) die "--token 格式不对，请从后台重新复制命令" ;; esac
[ ${#TOKEN} -eq 64 ] || die "--token 格式不对，请从后台重新复制命令"
case "$SERVER" in *[!A-Za-z0-9.:/_-]*) die "--server 含有非法字符" ;; esac
SERVER=${SERVER%/}
case "$SERVER" in
    https://?*) ;;
    http://127.0.0.1*|http://localhost*) ;;
    *) die "--server 必须是 https:// 地址，明文 http 只允许本机回环" ;;
esac

case "$(uname -m)" in
    x86_64|amd64) ARCH=amd64 ;;
    aarch64|arm64) ARCH=arm64 ;;
    *) die "暂不支持这个架构：$(uname -m)" ;;
esac

say "下载 agent（linux-$ARCH）……"
TMP=$(mktemp)
trap 'rm -f "$TMP" "$BIN.new"' EXIT
# 下载要带节点的 token，hub 不认的一律 404：程序不对外公开，扫描器也探不到这个地址。
URL="$SERVER/a/$ID/linux-$ARCH"
if command -v curl >/dev/null 2>&1; then
    curl -fsSL -H "Authorization: Bearer $TOKEN" -o "$TMP" "$URL" \
        || die "下载失败：token 不对，或者 hub 上还没放 linux-$ARCH 的 agent"
elif command -v wget >/dev/null 2>&1; then
    wget -q --header="Authorization: Bearer $TOKEN" -O "$TMP" "$URL" \
        || die "下载失败：token 不对，或者 hub 上还没放 linux-$ARCH 的 agent"
else
    die "需要 curl 或 wget"
fi
[ -s "$TMP" ] || die "下载到的文件是空的"

# 先装到旁边试跑一次，跑得起来才替换。/tmp 常被挂成 noexec，所以不在那里试。
install -m 755 "$TMP" "$BIN.new"
"$BIN.new" --version >/dev/null 2>&1 || die "下载的 agent 在这台机器上运行不了（系统太旧或架构不符）"

# 旧版本让 agent 跟 hub 共用 probe 用户。记下来，装好之后清理。
OLD_USER=$(sed -n 's/^User=//p' "$UNIT" 2>/dev/null || true)

if ! has_user "$AGENT_USER"; then
    useradd --system --user-group --no-create-home --home-dir /nonexistent \
        --shell /usr/sbin/nologin "$AGENT_USER" || die "创建 $AGENT_USER 用户失败"
fi

systemctl stop probe-agent >/dev/null 2>&1 || true
mv -f "$BIN.new" "$BIN"

# token 只放在 root 才能读的文件里。systemd 以 root 读取后再降权启动 agent，所以它不会出现在
# 进程命令行里；服务配置还把这个目录对 agent 设成不可见，agent 进程自己也读不到这个文件。
install -d -m 700 "$CONF_DIR"
umask 077
cat > "$ENV_FILE" <<EOF
PROBE_SERVER=$SERVER
PROBE_ID=$ID
PROBE_TOKEN=$TOKEN
EOF
chmod 600 "$ENV_FILE"

cat > "$UNIT" <<'EOF'
[Unit]
Description=Pulse probe agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=probe-agent
Group=probe-agent
EnvironmentFile=/etc/linux-probe/agent.env
ExecStart=/usr/local/bin/probe-agent
Restart=always
RestartSec=5
UMask=0077

# 权限：没有任何特权能力，也不能再提权
NoNewPrivileges=true
CapabilityBoundingSet=
RestrictSUIDSGID=true
LockPersonality=true
MemoryDenyWriteExecute=true
SystemCallFilter=@system-service
SystemCallArchitectures=native
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=true
RestrictRealtime=true
RemoveIPC=true

# 文件系统：整体只读；看不到家目录、hub 的数据库、自己的 token 文件
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
InaccessiblePaths=-/var/lib/linux-probe -/etc/linux-probe

# 内核与其他进程：只读 /proc/stat、/proc/meminfo 这类全局信息，看不到别的进程
ProtectProc=invisible
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectKernelLogs=true
ProtectControlGroups=true
ProtectClock=true
ProtectHostname=true

MemoryMax=64M
TasksMax=32

[Install]
WantedBy=multi-user.target
EOF
chmod 644 "$UNIT"
rm -rf "$DROPIN_DIR"

systemctl daemon-reload
systemctl enable probe-agent >/dev/null 2>&1
systemctl restart probe-agent
sleep 3

# 部分 LXC 容器不允许服务建挂载命名空间，上面那些文件系统隔离会让服务起不来（226/NAMESPACE）。
# 这时只撤掉依赖命名空间的那几项；非 root 用户、无特权能力、系统调用白名单这些照旧保留。
if ! systemctl is-active --quiet probe-agent \
    && [ "$(systemctl show -p ExecMainStatus --value probe-agent 2>/dev/null)" = 226 ]; then
    say "这台机器（多半是 LXC 容器）不支持文件系统隔离，改用兼容配置……"
    install -d -m 755 "$DROPIN_DIR"
    cat > "$DROPIN_DIR/container.conf" <<'EOF'
# 由安装脚本生成：容器不支持挂载命名空间。非 root 运行和系统调用白名单仍然生效。
[Service]
ProtectSystem=false
ProtectHome=false
PrivateTmp=false
PrivateDevices=false
InaccessiblePaths=
ProtectProc=default
ProtectKernelTunables=false
ProtectKernelModules=false
ProtectKernelLogs=false
ProtectControlGroups=false
ProtectClock=false
ProtectHostname=false
EOF
    chmod 644 "$DROPIN_DIR/container.conf"
    systemctl daemon-reload
    systemctl restart probe-agent
    sleep 3
fi

if ! systemctl is-active --quiet probe-agent; then
    say "agent 没能启动，看日志：journalctl -u probe-agent -n 30 --no-pager"
    exit 1
fi

# 从旧版升级上来的：probe 用户原来只给 agent 用，现在没人用了就删掉。
# 这台如果同时跑着 hub（probe-hub.service 在），probe 是 hub 的用户，不动。
if [ "$OLD_USER" = probe ] && [ ! -e /etc/systemd/system/probe-hub.service ] \
    && has_user probe && command -v pgrep >/dev/null 2>&1 && ! pgrep -u probe >/dev/null 2>&1; then
    userdel probe 2>/dev/null || true
fi

if [ -e "$DROPIN_DIR/container.conf" ]; then
    say "安装完成（容器兼容模式），agent 以 $AGENT_USER 用户运行。几秒后面板上就能看到这台机器。"
else
    say "安装完成，agent 以 $AGENT_USER 用户在沙箱里运行。几秒后面板上就能看到这台机器。"
fi
