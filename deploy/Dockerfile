# hub 的 Docker 镜像。不在镜像里编译：直接用发版流程编好的静态程序，镜像小、构建快。
# 两个架构的 agent 一起放进去，一键安装脚本从这里给 x86 和 ARM 机器下发。
FROM alpine:3.20

ARG TARGETARCH

# tzdata：「今日流量」「上午 9 点提醒」按 TZ 环境变量的时区算，没有它就只能是 UTC。
RUN apk add --no-cache tzdata \
 && adduser -D -H -u 10001 probe \
 && mkdir -p /data /usr/local/lib/probe \
 && chown probe:probe /data

COPY dist/probe-hub-linux-${TARGETARCH} /usr/local/bin/probe-hub
COPY dist/probe-agent-linux-amd64 dist/probe-agent-linux-arm64 /usr/local/lib/probe/
RUN chmod 755 /usr/local/bin/probe-hub /usr/local/lib/probe/probe-agent-linux-*

# 不以 root 运行；数据只写 /data 一个目录
# 命令行工具（admin-setup、access、backup）不用每次写 --db
ENV AVALON_DB=/data/probe.db

USER probe
VOLUME /data
EXPOSE 9911

# 健康检查只在容器内部访问，不经过反代，所以不受访问控制影响
HEALTHCHECK --interval=30s --timeout=3s CMD wget -qO- http://127.0.0.1:9911/health || exit 1

ENTRYPOINT ["/usr/local/bin/probe-hub"]
CMD ["serve", "--listen", "0.0.0.0:9911", "--db", "/data/probe.db"]
