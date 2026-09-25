# 发版

仓库：`github.com/merlin-node/Avalon`
镜像：`ghcr.io/merlin-node/avalon`

推一个 `v` 开头的标签，GitHub Actions 自动：跑测试 → 编译 x86_64 和 ARM64 静态程序 → 发布到 Releases（附 SHA256SUMS）→ 构建多架构镜像推到 `ghcr.io/merlin-node/avalon`，标签是版本号和 `latest`。

## 日常

```sh
git add -A
git commit -m "改了什么"
git push
```

每次推送都会自动跑一遍测试（Actions 里的 `ci`）。

## 发一个新版本

先把 `Cargo.toml` 里的 `version` 改大，提交，然后：

```sh
git tag v0.3.0
git push origin v0.3.0
```

Actions 里的 `release` 大约 10 分钟跑完。之后服务器上 `docker compose pull && docker compose up -d` 就是新版。

## 第一次发版后

打开仓库首页右侧的 **Packages → avalon → Package settings**，确认 **Visibility 是 Public**。否则服务器上 `docker compose pull` 会提示 `denied`。

## 失败时

打开 Actions 里红叉的那一次，点进失败的步骤看最后几十行。
