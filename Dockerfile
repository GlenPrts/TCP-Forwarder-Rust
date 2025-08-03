# =============================================================================
# Rust TCP智能转发器 - Docker构建文件
# =============================================================================

# 使用官方Rust镜像作为构建阶段
FROM rust:1.83-slim AS builder

# 设置工作目录
WORKDIR /app

# 安装必要的系统依赖
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# 首先复制Cargo文件以利用Docker缓存
COPY tcp-forwarder/Cargo.toml tcp-forwarder/Cargo.lock ./tcp-forwarder/

# 创建一个虚拟的main.rs文件来预编译依赖项
RUN mkdir -p tcp-forwarder/src && \
    echo "fn main() {}" > tcp-forwarder/src/main.rs

# 构建依赖项（这一步会被Docker缓存，除非Cargo.toml改变）
RUN cd tcp-forwarder && cargo build --release
RUN cd tcp-forwarder && rm src/main.rs

# 复制实际的源代码
COPY tcp-forwarder/src ./tcp-forwarder/src

# 构建最终的二进制文件
RUN cd tcp-forwarder && cargo build --release

# =============================================================================
# 运行时阶段 - 使用更小的基础镜像
# =============================================================================
FROM debian:bookworm-slim AS runtime

# 安装运行时依赖
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# 创建非root用户
RUN groupadd -r tcpforwarder && useradd -r -g tcpforwarder tcpforwarder

# 创建应用目录
RUN mkdir -p /app/config /app/logs && \
    chown -R tcpforwarder:tcpforwarder /app

# 从构建阶段复制二进制文件
COPY --from=builder /app/tcp-forwarder/target/release/tcp-forwarder /app/

# 复制默认配置文件
COPY tcp-forwarder/config.yaml /app/config/
COPY tcp-forwarder/ip_list.txt /app/config/

# 设置工作目录
WORKDIR /app

# 切换到非root用户
USER tcpforwarder

# 暴露端口
# 1234: TCP转发服务端口
# 9099: Prometheus指标端口
EXPOSE 1234 9099

# 设置环境变量
ENV RUST_LOG=info
ENV CONFIG_PATH=/app/config/config.yaml

# 健康检查
HEALTHCHECK --interval=30s --timeout=10s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:9099/metrics || exit 1

# 启动命令
CMD ["./tcp-forwarder", "--config", "/app/config/config.yaml"]
