# Avalon

自用的服务器状态面板。一个 hub 汇总多台 Linux 机器的实时状态、流量、延迟和续费到期，掉线和到期发 Telegram。hub 用 Docker 部署；agent 装在被监控的机器上主动上报，不开任何端口。两者都是 Rust 静态程序。

## 功能

- **实时状态**：CPU、内存、交换、磁盘、网速、负载、运行时间，每 5 秒一次；历史曲线 1 小时 / 6 小时 / 24 小时 / 7 天
- **流量**：按开机累加，重启不乱跳；按每台机器自己的重置日结算；月额度四种算法（相加、取大、仅上行、仅下行）；只算真实网卡；可手工校正
- **延迟**：后台添加 `host:port` 目标，指定节点测 TCP 握手，公开页显示曲线和丢包率
- **续费**：价格、周期、到期日；到期前 7 天起每天 9 点提醒；到期后仍在线可自动顺延
- **通知**：Telegram，掉线、恢复、到期、续期、新登录、备份下载和恢复
- **后台**：卡片折叠、节点 ↑↓ 排序、改账号密码、登录设备管理、备份下载与上传恢复、站点名称和图标；纯服务端页面，没有 JavaScript
- **家宽 / 动态 IP**：不需要公网 IP 或端口映射，换 IP 后自动重连

## 安全

- 可设**展示页域名**：这个域名上只有公开页；主域名上没登录的人看到的一律是 404
- **后台地址**可自定义，原来的 `/admin` 变成 404；所有拒绝都是同样的空 404
- 公开接口不含 IP、token、备注、真实节点编号、内核版本和监控目标
- 账号 + 密码（argon2id）；同一 IP 15 分钟内错 5 次锁 15 分钟；所有写操作校验同源和 CSRF
- 登录不满 24 小时的设备不能踢人、不能改账号密码（只剩它一个在线时除外）
- hub 容器只读、非 root、无特权；agent 以专用用户在 systemd 沙箱里运行

## 安装 hub

需要一台 Linux 服务器和一个解析到它的域名。镜像由 GitHub 编好，服务器上不编译。

没有 Docker 先装（已有就跳过）：

```sh
curl -fsSL https://get.docker.com | sh
```

建目录，写入 `docker-compose.yml`：

```sh
mkdir -p /opt/avalon && cd /opt/avalon
cat > docker-compose.yml <<'EOF'
services:
  avalon:
    image: ghcr.io/merlin-node/avalon:latest
    container_name: avalon
    restart: unless-stopped
    ports:
      - "127.0.0.1:9911:9911"
    volumes:
      - avalon-data:/data
    environment:
      TZ: Asia/Shanghai
    read_only: true
    tmpfs:
      - /tmp
    cap_drop:
      - ALL
    security_opt:
      - no-new-privileges:true

volumes:
  avalon-data:
EOF
```

启动，建管理员账号：

```sh
docker compose up -d
docker compose exec avalon probe-hub admin-setup
```

最后一条打印账号 `admin` 和一个随机密码，密码**只显示一次**。时区改 `TZ` 那一行，改完再 `docker compose up -d`。

**反向代理**：hub 只监听 `127.0.0.1:9911`，用 Caddy、nginx 等反代到它并配好 HTTPS。必须透传 WebSocket，并带上 `X-Forwarded-Host` 和 `X-Forwarded-For`（Caddy 默认就会，nginx 要手动加）。用展示页域名时，两个域名都反代到这同一个端口。

**首次登录**打开 `https://你的域名/admin`，然后：

1. 「账号」：改掉账号和密码，密码至少 12 位
2. 「访问控制」：设一个只有你知道的后台地址，保存后马上收藏
3. 「Telegram 通知」：填 Bot Token 和 Chat ID，发一条测试

## 添加节点

后台「添加节点」→ 填名字 → 把给出的命令在被监控的机器上用 root 执行。要求 Linux + systemd、x86_64 或 ARM64、能访问 hub 的域名。同一条命令重跑就是升级。

## 日常

**升级 hub**

```sh
cd /opt/avalon && docker compose pull && docker compose up -d
```

**升级 agent**：在节点上重跑后台里它的安装命令。

**备份**：后台「备份」→ 下载备份。文件里有所有节点 token 和 Bot Token，按密码保管。

**恢复**：后台「备份」→ 勾选确认 → 上传。hub 自动重启，约十秒后用**备份里的**账号密码和后台地址登录。原来的库留在 `probe.db.before-restore`。

后台打不开时用命令行：

```sh
cd /opt/avalon
# 备份
docker compose exec avalon probe-hub backup /data/backup.db
docker compose cp avalon:/data/backup.db ./avalon-backup.db
# 恢复
docker compose stop
docker compose run --rm -v "$PWD/avalon-backup.db:/import.db:ro" --entrypoint sh avalon \
  -c 'cp /import.db /data/probe.db && rm -f /data/probe.db-wal /data/probe.db-shm'
docker compose up -d
```

## 换机器

新机器用**同一个域名**，被控机什么都不用改。

1. 旧 hub 下载备份。早期 systemd 版用命令行：
   `runuser -u probe -- /usr/local/bin/probe-hub backup /tmp/avalon-backup.db --db /var/lib/linux-probe/probe.db`
2. 域名解析改到新机器
3. 新机器按上面装好 hub、配好反代，跑 `admin-setup` 登录
4. 「备份」上传恢复，十秒后用**旧的**账号密码、**旧的**后台地址登录
5. 被控机几分钟内自己重连上线
6. 按下面删掉旧 hub

## 卸载

**agent**：在节点上执行后台节点详情里的卸载命令（删服务、程序、`/etc/linux-probe` 和 `probe-agent` 用户），再在后台删除这个节点。

**hub（Docker）**，先备份：

```sh
cd /opt/avalon && docker compose down -v
docker image rm $(docker image ls -q ghcr.io/merlin-node/avalon)
cd / && rm -rf /opt/avalon
```

然后删掉反代里对应的站点。

**hub（早期 systemd 版）**，先备份：

```sh
unit=$(systemctl show -p FragmentPath --value probe-hub)
systemctl disable --now probe-hub
rm -f "$unit" /usr/local/bin/probe-hub
systemctl daemon-reload
rm -rf /var/lib/linux-probe
userdel probe
```

## 急救

在 hub 所在机器的 `/opt/avalon` 目录下执行。

- **忘了账号或密码**：`docker compose exec avalon probe-hub admin-setup`。账号回到 `admin`，密码换新，所有设备被踢出
- **忘了后台地址**：`docker compose exec avalon probe-hub access`
- **把自己关在外面**（域名或后台地址设错）：`docker compose exec avalon probe-hub access-reset && docker compose restart`。恢复成 `/admin`、不分域名、公开页开放
- **有人登了你的后台**：在旧设备的「登录设备」里踢掉他，再改密码；踢不动就直接跑 `admin-setup`
- **提示登录失败过多**：等 15 分钟，或换个网络
- **节点 token 泄露**：后台该节点点「重新生成 Token」，到那台机器上重跑新的安装命令
- **页面打不开**：`docker compose ps` 看容器状态，`docker compose logs --tail 50 avalon` 看日志
- **新版有问题**：把 `docker-compose.yml` 的 `image:` 改成 GitHub Packages 里上一个 `sha-xxxxxxx` 标签，再 `docker compose up -d`
- **节点一直离线**：在节点上看 `systemctl status probe-agent --no-pager` 和 `journalctl -u probe-agent -n 50 --no-pager`。多半是连不上 hub 域名，重跑安装命令

## 数据保留

| 数据 | 保留 |
|---|---|
| 图表（每分钟一条） | 7 天 |
| 延迟明细 | 3 天 |
| 每日流量 | 35 天 |
| 累计流量 | 永久 |

每小时清理一次，腾出的空间还给磁盘。

## 开发

推到 `main` 会自动测试并构建镜像 `ghcr.io/merlin-node/avalon`，详见 [`deploy/GITHUB.md`](deploy/GITHUB.md)。
