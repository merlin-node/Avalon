# Avalon

自用的服务器状态面板。一个 hub 汇总多台 Linux 机器的实时状态、流量、网络延迟和续费到期，在一个页面上看；掉线和到期通过 Telegram 提醒。

- **hub**：用 Docker Compose 部署，提供公开的状态页和网页后台，数据存在一个 SQLite 文件里
- **agent**：装在每台被监控的机器上，主动连接 hub 上报，不监听任何端口、不写任何文件

hub 和 agent 都用 Rust 编写，各自是一个静态链接的程序。

## 功能

### 实时状态

CPU、内存、交换、根分区、网速、1/5/15 分钟负载、运行时间，默认每 5 秒一次。首页是一张表格，点开任意一台看历史曲线（1 小时 / 6 小时 / 24 小时 / 7 天）。

### 流量统计

- 按**开机 ID** 累加：只计同一次开机内计数器的增长。机器重启、网卡变动、同一个 token 被两台机器共用时，只重新对基线，不会凭空多出几百 GB
- **本期流量按每台机器自己的重置日结算**（1–31 日，超过当月天数按月底算），今日流量按 hub 所在时区切日
- **每月额度**按 GB 填，四种计算方式：上下行相加、取较大值、仅上行、仅下行
- 只统计真实网卡，Docker、WireGuard、Tailscale 这类虚拟网卡不计入
- **流量校正**：换机器或迁移时，可以手工把总流量、本期流量改成商家后台的数字；本期校正只对当期有效

### 网络延迟

后台添加延迟监控：一个名字、一个 `host:port` 目标、间隔（5–3600 秒），再勾选由哪些节点去测。每个节点各自向目标发起 TCP 连接，记录握手用时，公开页上每个监控一条曲线，带波动区间和丢包率。目标地址只在后台可见。

### 续费与到期

每台机器可以填价格、货币（填什么显示什么，`$`、`CAD`、`元` 都行）、付款周期和到期日。

- 到期前 7 天起，每天上午 9 点发一次提醒，到期当天也发
- 过了到期日、机器仍在线、开着「自动续期」的，按付款周期自动往后顺延并通知；一次性付款的不顺延，离线的不顺延也不打扰

### 通知

Telegram，私聊或群组都行：节点掉线、恢复、即将到期、已自动续期。每台机器可以单独关掉。

### 后台

- 每张卡片都能折叠，默认只展开「节点」；保存设置后自动展开刚才那张、滚到刚才那一行
- **节点排序**：每行右侧 ↑↓，公开页跟着变；延迟监控同样可以排，只影响后台
- **账号**：账号和密码都能在后台改，密码至少 12 位
- **登录设备**：列出所有登录着的设备，可以踢出；每次新登录发 Telegram 通知
- **备份**：下载整库备份、上传恢复，换机器时被控机一台都不用动
- 纯服务端页面，不带一行 JavaScript

### 其他

- **自动识别 IP**：agent 报告本机的公网 IPv4 / IPv6；家宽这种在 NAT 后面的，用 hub 看到的出口地址。只在后台显示
- **家宽、动态 IP**：不需要公网 IP、DDNS 或端口映射；换 IP 后约半分钟内自动重连
- **站点名称和图标**可以在后台改，内置两个图标，也可以上传自己的

## 访问控制与安全

后台「访问控制」里有三项：

- **展示页域名**：填了以后，这个域名上只有公开状态页；另一个域名（主域名）上，**没登录的人看到的所有地址都是 404**
- **后台地址**：后台页面和全部后台接口一起搬到你自定义的路径，原来的 `/admin` 也变成 404
- **公开页开关**：关掉后展示页域名也全部 404

所有拒绝都是同一个空的 404，和「地址不存在」没有区别。另外：

- agent 的安装地址带有由 token 派生的钥匙，程序下载和上报连接都要验证 token，不对一律 404
- 公开接口不含 IP、token、备注、真实节点编号、内核版本和监控目标
- 后台「登录设备」列出所有登录会话，可以踢出，每次新登录发 Telegram 通知；登录不满 24 小时的设备不能踢人、不能改账号密码（只有它一个在线时除外），防止别人偷到密码后反手把你踢掉
- 后台要账号加密码，密码用 argon2id 保存；错账号和错密码的提示一样，同一来源 15 分钟内错 5 次锁定 15 分钟；所有写操作要求同源和 CSRF 令牌
- hub 容器：只读文件系统、不以 root 运行、去掉全部特权能力
- agent：以专用的 `probe-agent` 用户在 systemd 沙箱里运行，看不到自己的 token 文件，也看不到本机其他进程；在不支持沙箱的 LXC 容器里自动改用兼容配置，仍不以 root 运行
- 上传图标按文件内容识别格式，不收 SVG

## 部署

### 安装 hub

需要一台装了 Docker 的 Linux 服务器，以及一个指向它的域名。镜像由 GitHub 编译好，服务器上**不编译**，小机器也扛得住。

```sh
mkdir -p /opt/avalon && cd /opt/avalon
curl -fsSLO https://raw.githubusercontent.com/merlin-node/Avalon/main/docker-compose.yml
docker compose up -d
docker compose exec avalon probe-hub admin-setup
```

最后一条会打印管理员账号 `admin` 和一个随机密码，密码**只显示一次**，存进你的密码库。

`docker-compose.yml` 里的 `TZ` 默认是 `Asia/Shanghai`，在别的时区就改掉，改完 `docker compose up -d`。

### 反向代理

hub 只监听 `127.0.0.1:9911`，由反向代理负责 HTTPS。以 Caddy 为例：

```caddyfile
status.example.com {
    reverse_proxy 127.0.0.1:9911
}
```

两个域名（主域名和展示页域名）就写两段，都指向 `127.0.0.1:9911`。反向代理需要：

- 透传 WebSocket（agent 用它上报）。Caddy 默认支持；nginx 要加 `Upgrade` 和 `Connection` 头
- 带上 `X-Forwarded-Host` 和 `X-Forwarded-For`。Caddy 默认会带；nginx 要手动加。访问控制和登录限流都靠它们

### 登录后台

浏览器打开 `https://你的域名/admin`，用上面的账号和密码登录。后台每张卡片默认是收起的，点标题展开。建议先做这几件事：

1. **账号**：改成自己的账号和密码，密码至少 12 位；保存后所有会话失效，要重新登录
2. **站点**：改名字、选图标
3. **访问控制**：设置一个只有你知道的后台地址，保存后页面会跳过去，马上收藏；需要的话再填展示页域名
4. **Telegram 通知**：填 Bot Token 和 Chat ID，保存后发一条测试

### 添加节点

后台「添加节点」，填个名字，页面会给出一条安装命令。到要监控的机器上用 root 执行，几秒后首页就会出现这台机器。这条命令之后在节点详情里随时能再看到，**重复执行就是升级**。

要求：

- Linux，带 systemd（Debian、Ubuntu、Armbian 等）
- x86_64 或 ARM64
- 能访问 hub 的域名（HTTPS）

然后在节点详情里填上国家/地区代码（两个字母，如 `HK`、`JP`，公开页会显示旗子）、到期日、流量额度等。

## 日常维护

### 升级

hub：

```sh
cd /opt/avalon
docker compose pull
docker compose up -d
```

agent：在后台复制各节点的安装命令，到对应机器上重跑一次。

### 备份与恢复

后台「备份」卡片：

- **下载备份**：整个数据库一个 `.db` 文件，全部设置、节点、token、历史数据都在里面
- **上传恢复**：勾选「覆盖现有全部数据」后上传，hub 自动重启换上，大约十秒后刷新。之后要用**备份里的**账号密码登录，后台地址也变回备份里的那个；所有设备都要重新登录

原来的数据库会留一份 `probe.db.before-restore`，恢复错了可以换回来。下载和恢复都会发 Telegram 通知。备份里有所有节点的 token 和 Bot Token，请按密码一样保管。

自动重启靠 Docker 的 `restart: unless-stopped`（`docker-compose.yml` 里已经写好）。自己用 systemd 跑 hub 的，服务里要有 `Restart=always`。

后台打不开时用命令行，备份：

```sh
cd /opt/avalon
docker compose exec avalon probe-hub backup /data/backup.db
docker compose cp avalon:/data/backup.db ./avalon-backup.db
```

恢复：

```sh
cd /opt/avalon
docker compose stop
docker compose run --rm -v "$PWD/avalon-backup.db:/import.db:ro" --entrypoint sh avalon \
  -c 'cp /import.db /data/probe.db && rm -f /data/probe.db-wal /data/probe.db-shm'
docker compose up -d
```

### 换机器

和 Komari 一样，**新机器用同一个域名**，被控机上什么都不用改，它们会自己连到新机器：

1. 旧后台「备份」→ 下载备份
2. 把域名解析改到新机器
3. 新机器按上面「安装 hub」装好、配好反向代理，跑 `admin-setup` 登录
4. 「备份」→ 上传恢复，十秒后用**旧的**账号密码、**旧的**后台地址登录
5. 等几分钟，被控机陆续自己重连上线
6. 都在线了，按下面「彻底删除」清掉旧主控

### 卸载与彻底删除

**被控机**：后台节点详情里有一行卸载命令，到那台机器上用 root 执行。它会停掉服务，删掉程序、服务文件、`/etc/linux-probe` 配置和 `probe-agent` 用户。然后在后台删除这个节点。

**主控（Docker 部署）**，先下载一份备份再动手：

```sh
cd /opt/avalon
docker compose down -v                                        # 删容器和数据
docker image rm $(docker image ls -q ghcr.io/merlin-node/avalon)   # 删镜像
cd / && rm -rf /opt/avalon                                    # 删目录
```

然后删掉反向代理里 Avalon 那段配置并重载，不用的域名解析也删掉。

**主控（早期用 systemd 直接跑的）**：

```sh
unit=$(systemctl show -p FragmentPath --value probe-hub)
systemctl disable --now probe-hub
rm -f "$unit" /usr/local/bin/probe-hub
systemctl daemon-reload
rm -rf /var/lib/linux-probe       # 数据，确认备份过再删
userdel probe
```

**GitHub**：推送用的令牌不用了就在 Settings → Developer settings → Personal access tokens 里删掉。

## 急救指令

下面的命令都在 hub 所在的服务器上执行，先 `cd /opt/avalon`。

### 忘了账号或密码

```sh
docker compose exec avalon probe-hub admin-setup
```

账号回到 `admin`，打印一个新密码（只显示一次），所有已登录的设备同时被踢出。

### 忘了后台地址

```sh
docker compose exec avalon probe-hub access
```

### 把自己关在外面（展示页域名填错、后台地址打不开）

```sh
docker compose exec avalon probe-hub access-reset
docker compose restart
```

恢复成 `/admin`、不分域名、公开页开放。

### 怀疑别人登录了你的后台

有旧设备在手：后台「登录设备」里踢出不认识的，然后在「账号」里改密码。

旧设备也没了，或者新设备不满 24 小时踢不动：直接跑一次 `admin-setup`，所有设备全部踢出、密码换新。

### 登录提示「失败次数过多」

同一个 IP 15 分钟内错 5 次会锁 15 分钟，等 15 分钟自动解开，或者换个网络。

### 某个节点的 token 泄露了

后台展开那个节点，点「重新生成 Token」，旧 token 立刻失效；到那台机器上重跑一次新的安装命令。

### 后台或公开页打不开

```sh
docker compose ps                      # 看容器是不是 running
docker compose logs --tail 50 avalon   # 看最后 50 行日志
```

### 新版有问题，退回上一个

见 [`deploy/GITHUB.md`](deploy/GITHUB.md) 的「退回上一个」。

### 某台节点一直离线

在那台机器上：

```sh
systemctl status probe-agent --no-pager
journalctl -u probe-agent -n 50 --no-pager
```

多数是连不上 hub 的域名，或者 token 被换过。重跑一次后台给的安装命令通常就好。

## 数据保留

| 数据 | 保留 |
|---|---|
| 图表样本（每分钟一条） | 7 天 |
| 延迟明细 | 3 天 |
| 每日流量 | 35 天 |
| 累计流量 | 一直保留 |

每小时清理一次过期数据，腾出来的空间会还给磁盘，数据库文件不会只涨不跌。

## 从源码构建

打开 `docker-compose.yml` 里注释掉的 `build` 那三行、注释掉 `image` 那一行，然后：

```sh
docker compose up -d --build
```

源码构建会在服务器上编译 Rust，小内存机器可能扛不住，一般用不着。源码构建的镜像只带本机架构的 agent。

推送、自动构建和发版见 [`deploy/GITHUB.md`](deploy/GITHUB.md)。

## 致谢

公开状态页使用 [ServerStatus 主题](https://github.com/syuim/monitor-theme-serverstatus)（MIT），许可见 `theme/LICENSE`；国旗图标来自 flag-icons（MIT），许可见 `theme/FLAG-ICONS-LICENSE`。
