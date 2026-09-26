# ServerStatus theme integration

The source in this directory comes from `syuim/monitor-theme-serverstatus`, revision `995061c3a24fc07e7e4535dfb424209495b712b5`. Its MIT license is in `LICENSE`. Flag artwork is licensed separately in `FLAG-ICONS-LICENSE`.

`src/lib/api.ts` maps the Rust hub's public node and history endpoints into the upstream theme's data model. The process/connection line in `src/components/ServerTable.tsx` displays an unknown value when the hub does not collect it. The upstream visual layout, styling, chart widgets and navigation components remain in use.

The agent measures a TCP connection latency to its Hub every minute. When an administrator configures a per-node TCP target, the Hub instead measures that target and supplies its history to the upstream latency widget. This differs from the upstream multiple probes dispatched to agents. The Rust Hub implements its own compact admin panel to edit node and billing metadata, per-node TCP targets and Telegram alerts. Country codes come from the hub: a manual value set in the admin panel, otherwise the `CF-IPCountry` Cloudflare attaches to the agent's connection. There is no third-party geolocation lookup, multiple configurable agent probes, or upstream admin panel implementation. Unknown billing and quota values show as unconfigured, not as free, unlimited or never expiring. Do not fill these with fabricated example data.

## 构建

`dist/` 不进仓库。GitHub 构建时先在这个目录执行 `npm ci && npm run build`，再编译 hub，hub 把 `dist/` 嵌进程序里。

改公开页就改 `src/` 里的源码，推送即可，Vite 会自动给文件名加内容指纹，不用手动改名。本地编译 hub 之前，要先在这个目录执行一次上面那条命令。
