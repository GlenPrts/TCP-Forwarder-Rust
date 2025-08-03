# Docker部署指南

本文档说明如何使用Docker部署Rust TCP智能转发器。

## 文件说明

- `Dockerfile` - 多阶段构建的Docker镜像定义
- `docker-compose.yml` - 完整的Docker Compose配置（包含监控）
- `docker-compose.simple.yml` - 简化的Docker Compose配置（仅主服务）
- `.dockerignore` - Docker构建忽略文件
- `deploy.sh` - 自动化部署脚本

## 快速开始

### 方法1：使用部署脚本（推荐）

```bash
# 检查必要文件
./deploy.sh check

# 构建并运行（简单模式）
./deploy.sh run

# 或使用Docker Compose
./deploy.sh compose
```

### 方法2：手动构建和运行

```bash
# 构建镜像
docker build -t tcp-forwarder:latest .

# 运行容器
docker run -d \
  --name tcp-forwarder \
  --restart unless-stopped \
  -p 1234:1234 \
  -p 9099:9099 \
  -v $(pwd)/tcp-forwarder/config.yaml:/app/config/config.yaml:ro \
  -v $(pwd)/tcp-forwarder/ip_list.txt:/app/config/ip_list.txt:ro \
  -v $(pwd)/logs:/app/logs \
  -e RUST_LOG=info \
  tcp-forwarder:latest
```

### 方法3：使用Docker Compose

```bash
# 简化部署（仅主服务）
docker-compose -f docker-compose.simple.yml up -d

# 完整部署（包含Prometheus和Grafana监控）
docker-compose up -d
```

## 端口映射

- `1234` - TCP转发服务端口
- `9099` - Prometheus指标端口
- `9090` - Prometheus Web UI（仅完整部署）
- `3000` - Grafana仪表板（仅完整部署）

## 卷挂载

- `./tcp-forwarder/config.yaml` → `/app/config/config.yaml` (只读)
- `./tcp-forwarder/ip_list.txt` → `/app/config/ip_list.txt` (只读)
- `./logs` → `/app/logs` (读写)

## 环境变量

- `RUST_LOG` - 日志级别（默认: info）
- `CONFIG_PATH` - 配置文件路径（默认: /app/config/config.yaml）

## 监控访问

启动完整部署后，可以访问：

- **Prometheus**: http://localhost:9090
- **Grafana**: http://localhost:3000 (admin/admin)
- **应用指标**: http://localhost:9099/metrics

## 常用命令

```bash
# 查看运行状态
docker ps | grep tcp-forwarder

# 查看日志
docker logs -f tcp-forwarder
# 或
./deploy.sh logs

# 停止服务
docker stop tcp-forwarder
# 或
./deploy.sh stop

# 重启服务
docker restart tcp-forwarder

# 进入容器
docker exec -it tcp-forwarder /bin/bash

# 查看资源使用
docker stats tcp-forwarder
```

## 健康检查

容器包含健康检查，会定期检查指标端点：

```bash
# 查看健康状态
docker inspect tcp-forwarder | grep -A 5 Health
```

## 故障排除

### 构建失败
- 检查网络连接
- 确保有足够的磁盘空间
- 检查Rust版本兼容性

### 运行失败
- 检查配置文件格式
- 检查端口是否被占用
- 查看容器日志: `docker logs tcp-forwarder`

### 配置更新
```bash
# 修改配置后重启
docker restart tcp-forwarder

# 或重新部署
./deploy.sh stop
./deploy.sh run
```

## 生产环境建议

1. **资源限制**: 在docker-compose.yml中设置适当的内存和CPU限制
2. **日志轮转**: 配置Docker日志驱动进行日志轮转
3. **持久化**: 对于重要数据，使用Docker volumes而非bind mounts
4. **安全**: 使用非root用户运行（已在Dockerfile中配置）
5. **网络**: 在生产环境中使用自定义网络
6. **备份**: 定期备份配置文件和日志

## 镜像信息

- **基础镜像**: debian:bookworm-slim
- **运行用户**: tcpforwarder (非root)
- **镜像大小**: 约50-100MB（压缩后）
- **架构支持**: linux/amd64

## 自定义构建

如需自定义构建参数：

```bash
# 指定Rust版本
docker build --build-arg RUST_VERSION=1.83 -t tcp-forwarder:custom .

# 启用优化
docker build --build-arg BUILD_MODE=release -t tcp-forwarder:optimized .
```
