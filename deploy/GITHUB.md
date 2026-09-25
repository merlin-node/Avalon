# 推送与发版

仓库：`github.com/merlin-node/Avalon`
镜像：`ghcr.io/merlin-node/avalon`

## 每次推送会发生什么

推到 `main` 后，Actions 里的 **build** 自动：跑测试 → 编译 x86_64 和 ARM64 静态程序 → 打多架构镜像推到 `ghcr.io/merlin-node/avalon`，标签是：

- `latest`：永远是最新一次推送
- `sha-提交号前七位`：每次推送各一个，用来退回

大约 10–15 分钟跑完。之后服务器上：

```sh
cd /opt/avalon
docker compose pull && docker compose up -d
```

测试不过，后面的编译和镜像都不会跑，`latest` 不会被坏代码覆盖。

## 日常推送

```sh
git add -A
git commit -m "改了什么"
git push
```

## 发一个正式版本（可选）

想在 Releases 页面留一个带版本号的下载，先把 `Cargo.toml` 里的 `version` 改大，提交推送，然后：

```sh
git tag v0.3.0
git push origin v0.3.0
```

Actions 里的 **release** 会发布程序文件（附 SHA256SUMS），并打一个 `v0.3.0` 标签的镜像。

## 第一次推送后

打开仓库首页右侧的 **Packages → avalon → Package settings**，确认 **Visibility 是 Public**。否则服务器上 `docker compose pull` 会提示 `denied`。

## 新版有问题，退回上一个

在 Packages 页面找到上一个能用的 `sha-xxxxxxx` 标签，把 `docker-compose.yml` 里的 `image:` 改成：

```yaml
    image: ghcr.io/merlin-node/avalon:sha-xxxxxxx
```

然后 `docker compose up -d`。修好之后再改回 `latest`。

## 失败时

打开 Actions 里红叉的那一次，点进失败的步骤看最后几十行。

- **检查 Cargo.lock** 失败：仓库里缺 `Cargo.lock`，从能编译的机器上拷一份进仓库再推
- **测试** 失败：代码有问题，看报错是哪个测试
- **镜像** 那一步提示没有权限：仓库 Settings → Actions → General → Workflow permissions 选 **Read and write permissions**
