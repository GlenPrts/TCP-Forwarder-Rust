# TCP Forwarder

一个基于 Rust 的高性能 TCP 流量转发器，支持动态连接池、负载均衡、健康检查和指标监控。

## 功能特性

### 核心功能
- **TCP 流量转发**: 高性能异步 TCP 连接转发
- **动态连接池**: 根据流量自动调整连接池大小
- **负载均衡**: 支持多种负载均衡算法
- **健康检查**: 自动检测后端服务健康状态
- **指标监控**: Prometheus 兼容的指标收集

### 高级功能
- **EWMA 评分系统**: 基于指数加权移动平均的服务器评分
- **智能选择器**: 动态选择最优后端服务器
- **优雅关闭**: 支持信号处理和优雅关闭
- **配置文件支持**: 支持命令行指定配置文件路径

## 快速开始

### 安装要求

- Rust 1.70+ 
- Tokio 异步运行时

### 编译

```bash
cd tcp-forwarder
cargo build --release
```

### 运行

```bash
# 使用默认配置文件 (config.yaml)
./target/release/tcp-forwarder

# 指定配置文件路径
./target/release/tcp-forwarder --config /path/to/config.yaml
./target/release/tcp-forwarder -c ./my-config.yaml

# 查看帮助信息
./target/release/tcp-forwarder --help
```

## 配置说明

### 配置文件示例

```yaml
# 服务器配置
server:
  listen_addr: "0.0.0.0:8080"

# 日志配置
logging:
  level: "info"          # trace, debug, info, warn, error
  format: "pretty"       # pretty, json
  output: "stdout"       # stdout, stderr, 或文件路径

# 远程服务器配置
remotes:
  default_remote_port: 80
  provider:
    provider_type: "file"
    file:
      path: "ip_list.txt"
  
  # 评分配置
  scoring:
    alpha: 0.1
    failure_penalty: 1000.0
    max_history_size: 100
    historical_bonus: 0.1
  
  # 选择器配置
  selector:
    algorithm: "ewma"     # ewma, random, round_robin
    probe_interval: 5     # 探测间隔(秒)
    probe_timeout: 2      # 探测超时(秒)
    max_failures: 3       # 最大失败次数
    recovery_time: 60     # 恢复时间(秒)

# 连接池配置
pools:
  algorithm: "least_connections"  # least_connections, round_robin, random
  default_pool_size: 10
  min_pool_size: 5
  max_pool_size: 50
  connection_timeout: 5           # 连接超时(秒)
  keepalive_idle: 60             # TCP keepalive 空闲时间(秒)
  keepalive_interval: 30         # TCP keepalive 间隔(秒)
  keepalive_retries: 3           # TCP keepalive 重试次数

# 指标监控配置
metrics:
  enabled: true
  listen_addr: "0.0.0.0:9090"
  path: "/metrics"
```

### IP 列表文件 (ip_list.txt)

```
# 后端服务器列表
192.168.1.10
192.168.1.11
192.168.1.12
# 支持注释行
10.0.0.100
```

## 负载均衡算法

### 1. 最少连接数 (least_connections)
选择当前活跃连接数最少的服务器。适合处理时间差异较大的请求。

### 2. 轮询 (round_robin)
按顺序轮流选择服务器。适合处理时间相近的请求。

### 3. 随机 (random)
随机选择服务器。适合服务器性能相近的场景。

## 服务器选择算法

### EWMA (指数加权移动平均)
基于以下指标进行综合评分：
- **延迟**: 请求响应时间
- **抖动**: 延迟变化程度  
- **成功率**: 请求成功比例
- **失败惩罚**: 连续失败的惩罚机制
- **历史奖励**: 长期稳定的奖励机制

评分公式：
```
score = latency_score + jitter_score + success_rate_score + failure_penalty + historical_bonus
```

## 监控指标

### 连接指标
- `tcp_forwarder_active_connections`: 当前活跃连接数
- `tcp_forwarder_total_connections`: 总连接数
- `tcp_forwarder_failed_connections`: 失败连接数
- `tcp_forwarder_connection_duration`: 连接持续时间

### 池指标
- `tcp_forwarder_pool_size`: 连接池大小
- `tcp_forwarder_pool_hits`: 连接池命中次数
- `tcp_forwarder_pool_misses`: 连接池未命中次数
- `tcp_forwarder_transfer_bytes`: 传输字节数

### 性能指标
- `tcp_forwarder_uptime`: 系统运行时间
- `tcp_forwarder_errors`: 错误计数
- `tcp_forwarder_server_score`: 服务器评分

访问 `http://localhost:9090/metrics` 查看详细指标。

## 项目结构

```
tcp-forwarder/
├── src/
│   ├── main.rs          # 主入口，命令行参数解析
│   ├── config.rs        # 配置管理
│   ├── pools.rs         # 连接池管理
│   ├── metrics.rs       # 指标收集
│   ├── scorer.rs        # 评分系统
│   ├── selector.rs      # 服务器选择
│   ├── probing.rs       # 健康检查
│   └── loadbalancer.rs  # 负载均衡
├── Cargo.toml           # 项目依赖
├── config.yaml          # 配置文件
└── ip_list.txt          # IP 列表文件
```

## 开发指南

### 运行测试

```bash
# 运行所有测试
cargo test

# 运行特定模块测试
cargo test pools::tests
cargo test scorer::tests

# 启用日志输出的测试
RUST_LOG=debug cargo test
```

### 开发模式运行

```bash
# 使用开发配置运行
RUST_LOG=debug cargo run -- --config config.yaml

# 热重载开发（需要安装 cargo-watch）
cargo install cargo-watch
cargo watch -x 'run -- --config config.yaml'
```

### 性能分析

```bash
# 编译优化版本
cargo build --release

# 使用 perf 分析性能
perf record --call-graph=dwarf ./target/release/tcp-forwarder
perf report
```

## 部署建议

### 系统优化

```bash
# 增加文件描述符限制
ulimit -n 65536

# 优化网络参数
echo 'net.core.somaxconn = 65536' >> /etc/sysctl.conf
echo 'net.core.netdev_max_backlog = 5000' >> /etc/sysctl.conf
echo 'net.ipv4.tcp_max_syn_backlog = 65536' >> /etc/sysctl.conf
sysctl -p
```

### Systemd 服务

创建 `/etc/systemd/system/tcp-forwarder.service`:

```ini
[Unit]
Description=TCP Forwarder
After=network.target

[Service]
Type=simple
User=tcp-forwarder
WorkingDirectory=/opt/tcp-forwarder
ExecStart=/opt/tcp-forwarder/tcp-forwarder --config /etc/tcp-forwarder/config.yaml
Restart=always
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
```

启动服务：
```bash
sudo systemctl daemon-reload
sudo systemctl enable tcp-forwarder
sudo systemctl start tcp-forwarder
```

## 故障排除

### 常见问题

1. **连接被拒绝**
   ```bash
   # 检查后端服务器状态
   telnet <backend_ip> <backend_port>
   
   # 检查防火墙
   iptables -L
   ```

2. **高延迟**
   ```bash
   # 检查网络延迟
   ping <backend_ip>
   
   # 查看连接池状态
   curl http://localhost:9090/metrics | grep pool
   ```

3. **内存占用过高**
   ```bash
   # 监控内存使用
   ps aux | grep tcp-forwarder
   
   # 调整连接池大小
   # 在配置文件中减少 max_pool_size
   ```

### 日志分析

```bash
# 查看错误日志
RUST_LOG=error ./tcp-forwarder --config config.yaml

# 查看详细日志
RUST_LOG=debug ./tcp-forwarder --config config.yaml

# 查看特定模块日志
RUST_LOG=tcp_forwarder::pools=debug ./tcp-forwarder --config config.yaml
```

## 性能测试

### 基准测试

```bash
# 使用 wrk 进行性能测试
wrk -t12 -c400 -d30s http://localhost:8080/

# 使用 ab 进行测试
ab -n 10000 -c 100 http://localhost:8080/
```

### 监控性能

```bash
# 实时查看指标
watch -n 1 'curl -s http://localhost:9090/metrics | grep tcp_forwarder'

# 使用 htop 监控系统资源
htop
```

## 贡献指南

1. Fork 项目
2. 创建特性分支 (`git checkout -b feature/amazing-feature`)
3. 提交更改 (`git commit -m 'Add amazing feature'`)
4. 推送分支 (`git push origin feature/amazing-feature`)
5. 创建 Pull Request

### 代码规范

- 使用 `cargo fmt` 格式化代码
- 使用 `cargo clippy` 检查代码质量
- 添加适当的单元测试
- 更新相关文档

## 许可证

MIT License - 详见 [LICENSE](LICENSE) 文件

## 更新日志

### v0.1.0 (当前版本)
- 初始版本发布
- 基础 TCP 转发功能
- 动态连接池支持
- EWMA 评分系统
- 负载均衡算法
- Prometheus 指标集成
- 命令行参数支持

## 相关项目

- [HAProxy](http://www.haproxy.org/) - 高性能负载均衡器
- [Nginx](https://nginx.org/) - 高性能 Web 服务器和反向代理
- [Envoy Proxy](https://www.envoyproxy.io/) - 云原生代理

## 联系方式

- 项目主页: https://github.com/GlenPrts/TCP-Forwarder-Rust
- 问题反馈: https://github.com/GlenPrts/TCP-Forwarder-Rust/issues