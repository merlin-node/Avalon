# Avalon

自用的服务器探针。主控（hub）跑在一台机器的 Docker 里，只通过 Cloudflare Tunnel 对外；被控（agent）装在每台要监控的机器上，主动连主控，不开任何端口。

## 功能

- **实时**：CPU、内存、交换、磁盘、网速、负载、运行时间，5 秒一次；曲线 1 小时 / 6 小时 / 24 小时 / 7 天
- **流量**：按每台机器自己的重置日结算；月额度四种算法（相加、取大、仅上行、仅下行）；可手工校正
- **延迟**：后台添加 `host:port` 目标，指定节点测 TCP 握手
- **续费**：价格、周期、到期日；到期前 7 天起每天 9 点提醒
- **Telegram**：掉线、恢复、到期、新登录、备份下载和恢复
- **后台**：卡片折叠、节点 ↑↓ 排序、改账号密码、登录设备、备份和恢复
- **地区**：自动识别每台机器所在国家并显示国旗，来源是 Cloudflare，不调用任何第三方网站；认错了（比如家宽开了透明代理）就在节点设置里手填，留空恢复自动
- **家宽 / 动态 IP**：不用公网 IP 和端口映射，换 IP 自动重连，显示的是真实出口 IP

## 安全

- **主域名**：没登录的人访问任何地址都是空白 404；后台只有自己设的随机地址能打开
- **展示页域名**：只有公开状态页，不含 IP、token、备注、内核版本、监控目标
- **走隧道**：服务器上不占任何端口，扫服务器 IP 找不到探针
- 登录错 3 次封 48 小时，按 Cloudflare 提供的真实 IP 算
- 登录不满 24 小时的设备不能踢人、不能改账号密码（只有它一个在线时除外）

---

## 从零安装

### 一、Cloudflare：建隧道

手机浏览器打开 `dash.cloudflare.com`：

1. 左边 **Zero Trust** → **网络** → **连接器** → **创建隧道** → 选 **Cloudflared** → 名字填 `avalon` → 保存
2. 页面会给出几段命令。点**最下面**那段 `cloudflared tunnel run --token …` 右上角的复制按钮
3. 粘贴到 Bitwarden 新建的笔记，删掉前面的 `cloudflared tunnel run --token `，**只留 `eyJ` 开头的那一串**，保存。这是隧道密钥，这一页别截图
4. 下一步会让填域名，**先不填**，直接关掉页面，隧道已经建好了

### 二、服务器：装探针

SSH 登录要当主控的机器。

**1. 确认有 Docker**

```sh
docker compose version
```

打印出版本号就跳过，报找不到命令就先装：

```sh
curl -fsSL https://get.docker.com | sh
```

**2. 建目录**

```sh
mkdir -p /opt/avalon && cd /opt/avalon
```

**3. 写入隧道密钥**

这一行**单独执行**，不要和别的命令一起粘贴。出现提示后粘贴 Bitwarden 里的密钥，屏幕上不显示，直接回车：

```sh
read -rsp "粘贴 Tunnel 密钥后回车: " t && echo "TUNNEL_TOKEN=$t" > .env && chmod 600 .env && unset t && echo
```

检查一下，不会显示密钥内容：

```sh
grep -q '^TUNNEL_TOKEN=eyJ' .env && echo 密钥格式正确 || echo 密钥不对
```

打印"密钥不对"就重新执行上一步。

**4. 下载配置、启动、建账号**

```sh
curl -fsSLO https://raw.githubusercontent.com/merlin-node/Avalon/main/docker-compose.yml
docker compose up -d
docker compose ps
docker compose exec avalon probe-hub admin-setup
```

- `ps` 要看到 `avalon` 和 `avalon-tunnel` 两个都是 `running`
- 最后一条打印账号 `admin` 和一个随机密码，**只显示一次，马上存进 Bitwarden**

**5. 回 Cloudflare 看隧道**

Zero Trust → 网络 → 连接器，`avalon` 的状态变成**正常**就对了。

### 三、Cloudflare：接上域名

**1. 删旧解析**

左上角菜单回到账户首页 → 点域名 → **DNS** → **记录**。要用的那两个子域名如果已经有记录，各点 **⋯** → 删除。没有就跳过。

**2. 在隧道里加域名**

Zero Trust → 网络 → 连接器 → 点 `avalon` → 顶部那排标签往右滑，点 **已发布应用程序路由**（**不是**「主机名路由」）→ 添加：

- **子域名**：主域名的前缀
- **域**：选你的域名
- **路径**：空着
- **类型**：`HTTP`
- **URL**：`avalon:9911`
- 「其他应用程序设置」**不要展开**

保存。展示页域名再加一条，填法完全一样，只是子域名不同。

DNS 记录会自动变成指向隧道的 `CNAME`，不用手动加。

**3. 强制 HTTPS**

回到域名 → **SSL/TLS** → **边缘证书** → 往下拉到 **始终使用 HTTPS** → 打开。

同一页下面的 **HSTS 不要开**，它作用于整个域名的所有子域名，开了很难撤。

### 四、登录后设置

浏览器打开 `https://主域名/admin`。**一定带 `https://`**，地址栏左边要是锁，是三角形说明走了明文。

1. **账号**：改成自己的账号和密码，密码至少 12 位。保存后会被踢下线，用新的重新登录
2. **访问控制**：
   - **后台地址**：填一串随机字符，保存后页面会跳过去，**马上收藏**。旧的 `/admin` 从此是 404
   - **展示页域名**：填展示页的完整域名
   - 勾上 **开放公开状态页**，保存
3. **Telegram 通知**：填 Bot Token 和 Chat ID，保存，点发送测试
4. **添加节点**：填名字 → 复制给出的命令 → 到那台机器上用 root 执行 → 回后台看它变成"在线"

**验证**，用 Safari 无痕浏览打开：

| 地址 | 应该看到 |
|---|---|
| `https://主域名/` | 空白 |
| `https://主域名/admin` | 空白 |
| `https://主域名/你的后台地址` | 登录页 |
| `https://展示页域名/` | 状态页 |

---

## 日常

**升级主控**

```sh
cd /opt/avalon && docker compose pull && docker compose up -d
```

**升级被控**：到那台机器上重跑后台里它的安装命令。

**备份**：后台「备份」→ 下载备份。文件里有所有节点 token 和 Bot Token，按密码保管。

**恢复**：后台「备份」→ 勾选「覆盖现有全部数据」→ 上传。十秒后用**备份里的**账号密码、**备份里的**后台地址登录。

## 换机器

隧道密钥不变，Cloudflare 里什么都不用改。

1. 旧后台「备份」→ 下载备份，传到新机器，放在 `/root/avalon-backup.db`
2. **先停旧的**，不然两台会同时接在同一个隧道上：

   ```sh
   cd /opt/avalon && docker compose down
   ```

3. 新机器按「二、服务器：装探针」做到 `curl` 下载配置那一行为止，**不要 `up`**，然后：

   ```sh
   chmod 644 /root/avalon-backup.db
   docker compose run --rm -v /root/avalon-backup.db:/import.db:ro --entrypoint sh avalon -c 'cp /import.db /data/probe.db'
   docker compose up -d
   ```

   不用跑 `admin-setup`，账号密码、后台地址、节点全在备份里
4. 被控机几分钟内自己连上。确认都在线后，按下面「卸载」把旧机器删干净

## 卸载

**被控机**：后台展开那个节点，复制卸载命令，到那台机器上用 root 执行，再在后台删除这个节点。

**主控**，先下载备份：

```sh
cd /opt/avalon && docker compose down -v
docker image rm $(docker image ls -q ghcr.io/merlin-node/avalon) $(docker image ls -q cloudflare/cloudflared)
cd / && rm -rf /opt/avalon
```

以后不再用了的话：Cloudflare 连接器里删掉 `avalon` 隧道，DNS 里删掉那两条 `CNAME`。

## 急救

都在主控机器的 `/opt/avalon` 目录下执行。

- **忘了账号或密码**：`docker compose exec avalon probe-hub admin-setup`。账号回到 `admin`，密码换新，所有设备被踢出
- **忘了后台地址**：`docker compose exec avalon probe-hub access`
- **把自己关在外面**（域名或后台地址设错）：`docker compose exec avalon probe-hub access-reset && docker compose restart`。恢复成 `/admin`、不分域名、公开页开放
- **登录被封**（错了 3 次）：`docker compose exec avalon probe-hub unban`
- **有人登了后台**：在旧设备的「登录设备」里踢掉他，再改密码；踢不动就直接跑 `admin-setup`
- **节点 token 泄露**：后台该节点点「重新生成 Token」，到那台机器上重跑新的安装命令
- **页面打不开**：`docker compose ps` 看两个容器是不是 `running`；`docker compose logs --tail 50 avalon` 看探针日志
- **Cloudflare 里隧道不是"正常"**：`docker compose logs --tail 30 tunnel`，多半是 `.env` 里的密钥不对，按「写入隧道密钥」重来一遍，再 `docker compose up -d`
- **新版有问题**：GitHub 仓库首页右侧 Packages → avalon，找上一个 `sha-xxxxxxx` 标签，把 `docker-compose.yml` 里 `ghcr.io/merlin-node/avalon:latest` 的 `latest` 改成它，再 `docker compose up -d`。修好后改回 `latest`
- **节点一直离线**：在那台节点上看 `systemctl status probe-agent --no-pager` 和 `journalctl -u probe-agent -n 50 --no-pager`。多半是连不上主域名，重跑安装命令。也可能被 Cloudflare 人机验证拦了：域名 → 安全性 → 规则里，让 `/api/agent/`、`/i/`、`/a/` 开头的路径跳过验证

## 数据保留

| 数据 | 保留 |
|---|---|
| 图表（每分钟一条） | 7 天 |
| 延迟明细 | 3 天 |
| 每日流量 | 35 天 |
| 累计流量 | 永久 |

每小时清理一次，腾出的空间还给磁盘。

## 改代码

见 [`deploy/GITHUB.md`](deploy/GITHUB.md)。公开页的源码在 `theme/src`，说明见 [`theme/INTEGRATION.md`](theme/INTEGRATION.md)。
