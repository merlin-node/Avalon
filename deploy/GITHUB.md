# 推送与发版

仓库 `github.com/merlin-node/Avalon`，镜像 `ghcr.io/merlin-node/avalon`。

## 每次推送会发生什么

推到 `main` 后，Actions 里的 **build** 自动：编译公开页主题 → 跑测试 → 编译 x86_64 和 ARM64 程序 → 打镜像推到 ghcr，标签是 `latest` 和 `sha-提交号前七位`。大约 4 分钟。

任何一步失败，后面都不会跑，`latest` 不会被坏代码覆盖。成功后到主控上 `cd /opt/avalon && docker compose pull && docker compose up -d`。

## 拿到更新包后推送

更新包是一个 zip，里面是改过的文件，路径都是相对仓库根目录的。在一台装了 git 的机器上（不用编译，小机器也行），把 zip 放到 `/root`：

```sh
cd /root && rm -rf avalon-push && mkdir avalon-push && cd avalon-push
git clone https://github.com/merlin-node/Avalon.git repo && cd repo
unzip -o /root/avalon-update.zip
git status --short
git -c user.name=merlin-node -c user.email=zsfnrly@gmail.com commit -am "改了什么"
git push
cd /root && rm -rf avalon-push avalon-update.zip
```

- `git status` 列出的 `M` 就是这次改的文件，数量要和包里的对上
- 推送时 Username 填 `merlin-node`，Password 粘贴 Bitwarden 里的 GitHub 令牌，屏幕不显示，直接回车
- 看到 `main -> main` 就成功了，去 Actions 看 build 是不是绿的
- 解压只会覆盖和新增文件，**不会删文件**。要删文件时会另外给一条 `git rm` 命令

## GitHub 令牌

推送用的令牌有效期 30 天，过期了推送会提示认证失败，重新建一个：

GitHub 右上角头像 → **Settings** → 最底下 **Developer settings** → **Personal access tokens** → **Fine-grained tokens** → **Generate new token**：

- **Expiration**：30 days
- **Repository access**：Only select repositories → 选 `Avalon`
- **Permissions → Repositories**：点 **+** 加 **Contents** 和 **Workflows**，都改成 **Read and write**
- 生成后复制 `github_pat_` 开头那串存进 Bitwarden，替换旧的

## Actions 红了怎么看

点开红叉的那一次 → 点红叉的任务 → 点红叉的那一步 → 看最后几十行。

## 退回上一个版本

仓库首页右侧 **Packages → avalon**，找上一个能用的 `sha-xxxxxxx` 标签，把主控 `docker-compose.yml` 里 `avalon:latest` 的 `latest` 换成它，`docker compose up -d`。修好后换回 `latest`。

## 正式版本（可选）

想在 Releases 页面留一个带版本号的下载：`Cargo.toml` 里的 `version` 改大，推送，然后 `git tag v0.3.0 && git push origin v0.3.0`。
