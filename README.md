# TCP Forwarder Rust

这是一个高性能的 TCP 转发器，旨在通过优选 IP（如 Cloudflare）转发流量。它采用扫描与转发分离的设计，结合基于 CIDR（IP 段）的智能评分与并发连接竞速机制，提供低延迟、高可靠的 TCP 转发服务。

## 核心特性

*   **基于 CIDR 的智能扫描**: 专门的扫描模式 (`--scan`)，自动将大网段拆分为子网（/24），并对每个子网进行采样评分，筛选优质 IP 段。
*   **灵活的转发策略**: 支持基于评分的 Top K% 筛选，并在转发时采用随机选段 + 随机选 IP 的两级随机策略，有效均衡负载并避免拥塞。
*   **持久化存储**: 扫描结果以子网为单位保存到本地文件，转发服务启动时直接加载。
*   **并发连接竞速**: 转发流量时，同时向多个优选 IP 段内的随机 IP 发起连接，使用最快建立的连接，显著降低握手延迟。
*   **Web 状态监控**: 内置 Web 服务器，实时查看当前 IP 段池状态和质量评分。
*   **故障自动转移**: 智能处理连接失败，自动尝试备用 IP。

## 快速开始

### 1. 环境准备

确保已安装 Rust 编程语言环境。如果没有，请访问 [rustup.rs](https://rustup.rs/) 安装。

### 2. 配置文件

在项目根目录下创建或修改 `config.json` 文件：

```json
{
  "target_port": 443,
  "target_colos": [
    "SJC",
    "LAX"
  ],
  "cidr_list": [
    "104.16.0.0/12",
    "172.64.0.0/13"
  ],
  "bind_addr": "0.0.0.0:8080",
  "web_addr": "0.0.0.0:3000",
  "trace_url": "http://engage.cloudflareclient.com/cdn-cgi/trace",
  "asn_url": "https://asn.0x01111110.com/13335?4",
  "ip_store_file": "subnet_results.json",
  "selection_top_k_percent": 0.1,
  "selection_random_n_subnets": 3,
  "selection_random_m_ips": 2
}
```

**配置项说明：**

*   `target_port`: 目标服务的端口（例如 Cloudflare 节点的 443 端口）。
*   `target_colos`: 期望的数据中心代码列表（如 "SJC", "LAX"）。如果留空 `[]`，则不限制数据中心。
*   `cidr_list`: 初始扫描的 IP 网段列表（会自动拆分为 /24 子网进行测试）。
*   `bind_addr`: 本地 TCP 转发服务的监听地址。
*   `web_addr`: 本地 Web 监控服务的监听地址。
*   `trace_url`: 用于测试 IP 延迟和获取 Colo 信息的 URL。
*   `asn_url`: 用于动态更新 CIDR 列表的 URL。
*   `ip_store_file`: 扫描结果的保存路径。
*   **策略参数**：
    *   `selection_top_k_percent`: 仅使用评分最高的前 `k`% 的 IP 段进行转发（例如 0.1 表示前 10%）。
    *   `selection_random_n_subnets`: 每次建立连接时，从优选 IP 段中随机选取的 IP 段数量。
    *   `selection_random_m_ips`: 在每个选中的 IP 段内，随机生成的 IP 数量。
    *   *说明：总并发连接尝试数 = n * m*

**注意**：配置文件现在支持自动验证。如果 `selection_top_k_percent` 不在 0-1 范围内，或 `selection_random_n_subnets`/`selection_random_m_ips` 小于等于 0，程序将拒绝启动并显示错误消息。

### 3. 使用步骤

本项目分为**扫描**和**转发**两个阶段。

#### 阶段一：扫描优选 IP 段

在使用转发功能前，需要先运行扫描模式来获取高质量 IP 段。扫描完成后，程序会自动退出并将结果保存到 `ip_store_file`。

```bash
# 开发模式
cargo run -- --scan

# 生产模式
./target/release/tcp-forwarder-rust --scan
```

#### 阶段二：启动转发服务

扫描完成后，直接运行程序即可启动转发服务。程序会加载之前的扫描结果，并根据配置的策略开始监听端口和转发流量。

```bash
# 开发模式
cargo run

# 生产模式
./target/release/tcp-forwarder-rust
```

#### 其他工具命令

查看数据中心质量排名（基于已有的扫描结果）：

```bash
cargo run -- --rank-colos
# 或
./target/release/tcp-forwarder-rust --rank-colos
```

此命令将按平均评分降序输出各数据中心的统计信息，帮助您决定 `config.json` 中 `target_colos` 的设置。

### 4. 构建生产版本

为了获得最佳性能，建议使用 release 模式构建：

```bash
cargo build --release
```

构建产物位于 `target/release/tcp-forwarder-rust` (Linux/macOS) 或 `target/release/tcp-forwarder-rust.exe` (Windows)。

## 最近改进

### v0.2.1 (2026-01-04)

**核心改进：**

1. **优雅关闭机制**：
   - 使用 `CancellationToken` 实现优雅关闭
   - TCP 服务器和 Web 服务器都支持优雅关闭
   - 关闭时等待现有连接完成（带超时保护）

2. **并发连接竞速优化**：
   - 使用 `mpsc::channel` 实现真正的并发竞速
   - 第一个成功的连接立即返回，其他连接被取消
   - 修复了之前顺序等待的问题

3. **代码重构**：
   - 新增 `src/utils.rs` 模块，消除重复代码
   - 改进命令行参数解析，添加 `--help` 支持
   - 使用 `parking_lot::RwLock` 替代 `std::sync::RwLock`

4. **类型安全改进**：
   - 将延迟和抖动类型从 `u128` 改为 `u64`（更合理的范围）
   - 添加自定义错误类型 `ConfigError`
   - 使用 `thiserror` 改进错误处理

5. **新增功能**：
   - 添加 `/health` 健康检查端点
   - 添加 `/api/v1/subnets` 新 API 端点（带元数据）
   - 数据中心排名显示更多统计信息（抖动、丢包率）

6. **测试覆盖**：
   - 新增 18 个单元测试
   - 覆盖配置验证、评分计算、IP 生成等核心功能

7. **构建优化**：
   - 添加 release profile 优化（LTO、单代码单元、strip）
   - 添加项目描述和许可证信息

**破坏性变更**：
- `SubnetQuality.avg_latency` 和 `avg_jitter` 类型从 `u128` 改为 `u64`
- 如果您有自定义的扫描结果文件，建议重新扫描

**迁移指南**：
- 现有配置文件继续兼容
- 建议重新运行 `--scan` 以更新扫描结果格式

### v0.2.0 (2026-01-03)

**核心改进：**

1. **错误处理增强**：
   - 改进配置文件加载失败时的错误日志
   - 添加配置参数自动验证，防止无效参数导致运行时错误

2. **性能优化**：
   - 使用 `tokio::select!` 替代忙等待模式，提高并发连接选择效率
   - 优化内存管理，预分配容器容量，减少重新分配开销
   - 使用 `Arc` 共享配置数据，减少不必要的克隆操作

3. **可观测性提升**：
   - 添加详细的调试日志，记录 IP 评分计算过程
   - 改进错误日志级别，更清晰地区分信息、警告和错误

4. **网络改进**：
   - 添加单独的连接超时（1秒），区分于总体请求超时（2秒）
   - 更细粒度的超时控制，提高网络操作响应性

5. **代码质量**：
   - 重构分析功能到独立模块 `src/analytics.rs`
   - 改进代码结构和模块化设计
   - 添加更完整的错误处理和资源清理

## 监控接口

服务启动后，可以访问 Web 接口查看当前加载的优选 IP 段列表及其质量评分：

*   地址: `http://localhost:3000/status` (默认)
*   返回格式: JSON 数组，按评分从高到低排序。

```json
[
  {
    "subnet": "1.2.3.0/24",
    "score": 85.5,
    "avg_latency": 120,
    "avg_jitter": 5,
    "avg_loss_rate": 0.0,
    "sample_count": 3,
    "colo": "LAX",
    "last_updated": "2023-10-27T10:00:00Z"
  },
  ...
]
