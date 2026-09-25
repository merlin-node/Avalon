# ServerStatus theme integration

The source in this directory comes from `syuim/monitor-theme-serverstatus`, revision `995061c3a24fc07e7e4535dfb424209495b712b5`. Its MIT license is in `LICENSE`. Flag artwork is licensed separately in `FLAG-ICONS-LICENSE`.

`src/lib/api.ts` maps the Rust hub's public node and history endpoints into the upstream theme's data model. The process/connection line in `src/components/ServerTable.tsx` displays an unknown value when the hub does not collect it. The upstream visual layout, styling, chart widgets and navigation components remain in use.

The agent measures a TCP connection latency to its Hub every minute. When an administrator configures a per-node TCP target, the Hub instead measures that target and supplies its history to the upstream latency widget. This differs from the upstream multiple probes dispatched to agents. The Rust Hub implements its own compact admin panel to edit node and billing metadata, per-node TCP targets and Telegram alerts. There is no automatic geolocation, multiple configurable agent probes, or upstream admin panel implementation. Unknown billing and quota values show as unconfigured, not as free, unlimited or never expiring. Do not fill these with fabricated example data.

The compiled `dist/` is included so a Debian VPS needs only Rust to build the probe. To rebuild the frontend from the source here, run `npm ci` and `npm run build` from this directory before compiling `probe-hub`.


## 直接改 dist 时必须改文件名

hub 对 `dist/assets/` 下的文件发一年的 `immutable` 缓存，因为 Vite 构建出来的文件名带内容指纹。
所以**手工修改 dist 里的 JS 时，文件名也要跟着改**（连同引用它的 index.html 和其他 chunk），
否则浏览器会一直用旧的那份。用 `npm run build` 重新构建则会自动换名，不用管。
