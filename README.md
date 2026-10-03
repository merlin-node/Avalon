<h1 align="center">Avalon</h1>

<p align="center">轻量、安全的自托管服务器探针</p>

<p align="center">
  <b>简体中文</b> | <a href="README_en.md">English</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT">
  <img src="https://img.shields.io/badge/Rust-static%20binary-orange" alt="Rust">
  <img src="https://img.shields.io/badge/Docker-ghcr.io-2496ED" alt="Docker">
  <img src="https://img.shields.io/github/actions/workflow/status/merlin-node/Avalon/build.yml?branch=main&label=build" alt="build">
</p>

---

Avalon 用来同时看管你的一批服务器：CPU、内存、硬盘、网速、流量、延迟、到期时间，一个页面看全，出了问题 Telegram 告诉你。

主控（hub）跑在 Docker 里，只通过 Cloudflare Tunnel 对外；被控（agent）装在每台机器上，主动连回主控。

## 特性

### 轻量

- 主控常驻内存不到 10 MB，镜像约 30 MB
- 被控常驻内存约 3 MB，程序约 2 MB
- 两者都是 Rust 编写的静态单文件，不依赖任何运行环境
- 被控不在本机存数据、不写硬盘，历史数据全部由主控保存

### 安全

- 不开任何端口，主控只通过 Cloudflare Tunnel 对外，被控只向外连接，扫描服务器 IP 找不到它
- 被控只上报数据，没有远程命令、终端、文件管理和自动更新，主控就算被攻破也碰不到你的机器
- 被控以专用的无特权用户运行，并受 systemd 沙箱限制
- 后台用自己设定的随机地址，其余地址一律返回空白 404，密码错 3 次封禁 48 小时
- 公开状态页和后台使用两个域名，访客看不到任何后台痕迹
- 国家和地区由 Cloudflare 识别，不把节点 IP 发给任何外部服务

## 部署

### 准备

- 一个托管在 Cloudflare 的域名，准备两个子域名：一个放后台（主域名），一个放公开状态页（展示页域名）
- 一台装了 Docker 的 Linux 服务器作为主控
- 被控支持 x86_64 / ARM64、使用 systemd 的 Linux

### 1. 创建隧道

Cloudflare 控制台 → **Zero Trust** → **网络** → **连接器** → **创建隧道** → **Cloudflared**，命名为 `avalon`。

在页面给出的安装命令里，复制 `--token` 后面以 `eyJ` 开头的那一串，这是隧道密钥。下一步询问域名时先跳过。

### 2. 启动主控

```sh
mkdir -p /opt/avalon && cd /opt/avalon
curl -fsSLO https://raw.githubusercontent.com/merlin-node/Avalon/main/docker-compose.yml
```

写入隧道密钥。**这一行单独执行**，粘贴密钥后回车，屏幕上不会显示：

```sh
read -rsp "Tunnel token: " t && echo "TUNNEL_TOKEN=$t" > .env && chmod 600 .env && unset t && echo
```

启动并创建管理员：

```sh
docker compose up -d
docker compose exec avalon probe-hub admin-setup
```

最后一条会打印账号 `admin` 和一个随机密码，**只显示一次**，请保存好。

### 3. 接上域名

回到隧道 `avalon` → **已发布应用程序路由** → 添加，两个子域名各加一条：

| 项 | 填写 |
|---|---|
| 子域名 | 主域名或展示页域名的前缀 |
| 类型 | `HTTP` |
| URL | `avalon:9911` |

DNS 记录会自动生成。建议在域名的 **SSL/TLS → 边缘证书** 里打开 **始终使用 HTTPS**。

### 4. 首次设置

打开 `https://主域名/admin` 登录，然后：

1. **账号**：改成自己的账号和密码
2. **访问控制**：设置一个随机的后台地址（保存后立即收藏，`/admin` 从此失效），填上展示页域名，开放公开状态页
3. **Telegram 通知**：填写 Bot Token 和 Chat ID，发送一条测试
4. **添加节点**：填写名称，把生成的命令复制到那台机器上用 root 执行，几秒后即显示在线

## 日常使用

**升级主控**

```sh
cd /opt/avalon && docker compose pull && docker compose up -d
```

**升级被控**：在那台机器上重新执行后台里它的安装命令，token 不变。

**备份与恢复**：后台「备份」卡片。备份文件含节点 token 和通知密钥，请妥善保管。

**常用命令**（在 `/opt/avalon` 下执行）

| 情况 | 命令 |
|---|---|
| 忘记密码 | `docker compose exec avalon probe-hub admin-setup` |
| 忘记后台地址 | `docker compose exec avalon probe-hub access` |
| 访问设置改错，进不去 | `docker compose exec avalon probe-hub access-reset && docker compose restart` |
| 登录被封禁 | `docker compose exec avalon probe-hub unban` |
| 查看日志 | `docker compose logs --tail 50 avalon` |

节点一直离线时，先在节点上查看 `journalctl -u probe-agent -n 50`。如果域名开启了 Cloudflare 人机验证，请让 `/api/agent/`、`/i/`、`/a/` 开头的路径跳过验证。

## 数据保留

| 数据 | 保留时长 |
|---|---|
| 资源图表 | 7 天 |
| 延迟明细 | 3 天 |
| 每日流量 | 35 天 |
| 累计流量 | 永久 |

## 致谢

[Claude](https://claude.ai)

## 许可证

[MIT](LICENSE)
