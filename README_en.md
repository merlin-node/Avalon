<h1 align="center">Avalon</h1>

<p align="center">A lightweight, secure, self-hosted server monitor</p>

<p align="center">
  <a href="README.md">简体中文</a> | <b>English</b>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT">
  <img src="https://img.shields.io/badge/Rust-static%20binary-orange" alt="Rust">
  <img src="https://img.shields.io/badge/Docker-ghcr.io-2496ED" alt="Docker">
  <img src="https://img.shields.io/github/actions/workflow/status/merlin-node/Avalon/build.yml?branch=main&label=build" alt="build">
</p>

---

Avalon keeps an eye on a fleet of servers: CPU, memory, disk, bandwidth, traffic, latency and renewal dates on one page, with Telegram alerts when something goes wrong.

The hub runs in Docker and is reachable only through a Cloudflare Tunnel. An agent on each server connects out to the hub.

> The interface is in Chinese.

## Features

### Lightweight

- The hub uses under 10 MB of memory, with an image of about 30 MB
- The agent uses about 3 MB of memory, with a binary of about 2 MB
- Both are static single binaries written in Rust, with no runtime to install
- The agent keeps nothing on disk. All history lives on the hub

### Secure

- No ports are opened. The hub is served only through Cloudflare Tunnel and agents only connect outward, so scanning the server IP finds nothing
- Agents only report data. There are no remote commands, terminal, file access or auto-update, so even a compromised hub cannot reach your servers
- The agent runs as a dedicated unprivileged user inside a systemd sandbox
- The panel lives at a random path you choose, every other path is a blank 404, and three wrong passwords block the address for 48 hours
- The public status page and the panel use separate domains, and visitors see no trace of the panel
- Country detection comes from Cloudflare, and node IPs are never sent to an outside service

## Deployment

### Requirements

- A domain on Cloudflare with two subdomains: one for the panel (main domain) and one for the public status page (status domain)
- A Linux server with Docker for the hub
- Agents run on x86_64 or ARM64 Linux with systemd

### 1. Create a tunnel

Cloudflare dashboard → **Zero Trust** → **Networks** → **Connectors** → **Create a tunnel** → **Cloudflared**, and name it `avalon`.

In the install command shown, copy the string after `--token` that starts with `eyJ`. This is the tunnel token. Skip the domain step for now.

### 2. Start the hub

```sh
mkdir -p /opt/avalon && cd /opt/avalon
curl -fsSLO https://raw.githubusercontent.com/merlin-node/Avalon/main/docker-compose.yml
```

Save the tunnel token. **Run this line on its own**, paste the token and press Enter. Nothing is echoed:

```sh
read -rsp "Tunnel token: " t && echo "TUNNEL_TOKEN=$t" > .env && chmod 600 .env && unset t && echo
```

Start it and create the administrator:

```sh
docker compose up -d
docker compose exec avalon probe-hub admin-setup
```

The last command prints the account `admin` and a random password **once**. Keep it safe.

### 3. Connect the domains

Back in the `avalon` tunnel → **Published application routes** → add one route for each subdomain:

| Field | Value |
|---|---|
| Subdomain | prefix of the main or status domain |
| Type | `HTTP` |
| URL | `avalon:9911` |

DNS records are created automatically. Turning on **Always Use HTTPS** under **SSL/TLS → Edge Certificates** is recommended.

### 4. First login

Open `https://<main domain>/admin`, sign in, then:

1. **账号 (Account)**: set your own username and password
2. **访问控制 (Access)**: set a random panel path (bookmark it right after saving, as `/admin` stops working), enter the status domain and open the public status page
3. **通知 (Notifications)**: under Telegram, enter the Bot Token and Chat ID and send a test; under 资源监控 (resource alerts) a 默认 (default) rule already exists, adjust its thresholds and machines as needed
4. **节点管理 → 新增节点 (Nodes → Add node)**: enter a name, run the generated command as root on that server, and it shows online within seconds

## Everyday use

**Update the hub**

```sh
cd /opt/avalon && docker compose pull && docker compose up -d
```

**Update an agent**: run its install command from the panel again on that server. The token stays the same.

**Backup and restore**: the 备份 (Backup) card in the panel. Backups contain node tokens and alert credentials, so store them safely.

**Useful commands** (run in `/opt/avalon`)

| Situation | Command |
|---|---|
| Forgot the password | `docker compose exec avalon probe-hub admin-setup` |
| Forgot the panel path | `docker compose exec avalon probe-hub access` |
| Locked out by access settings | `docker compose exec avalon probe-hub access-reset && docker compose restart` |
| Login blocked | `docker compose exec avalon probe-hub unban` |
| View logs | `docker compose logs --tail 50 avalon` |

If a node stays offline, check `journalctl -u probe-agent -n 50` on it. If Cloudflare bot challenges are enabled for the domain, let paths starting with `/api/agent/`, `/i/` and `/a/` skip them.

## Data retention

| Data | Kept for |
|---|---|
| Resource charts | 7 days |
| Latency details | 3 days |
| Daily traffic | 35 days |
| Total traffic | forever |

## Acknowledgements

[Claude](https://claude.ai)

## License

[MIT](LICENSE)
